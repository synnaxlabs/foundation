//! The proof of a subject: a signed hello, then signed requests on its connection.

use std::fmt;

use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::Name;
use types::node;
use types::time::{Interval, Span, Stamp};

use crate::Rules;

/// How far past the latest mesh time at its admission the node holds a hello live.
pub const CAP: Span = Span::from_nanos(15 * Span::MINUTE.nanos());

const HELLO_TAG: &[u8] = b"foundation/hello/1";
const REQUEST_TAG: &[u8] = b"foundation/request/1";

/// The bytes that the signature of `hello` covers. Each integer is little-endian:
/// the 18 bytes `foundation/hello/1`, the length of the subject (1 byte), the
/// subject, the key (32), `via` as a `u128` (16, the reverse of the byte order of its
/// UUID text), the connection (16), the nonce (16), and `expires` in nanoseconds (8).
/// After the tag, these are the bytes of [`Hello::encode`].
#[must_use]
pub fn hello(hello: &Hello) -> Vec<u8> {
    let mut bytes = vec![0; HELLO_TAG.len() + hello.encoded_len()];
    let (tag, fields) = bytes.split_at_mut(HELLO_TAG.len());
    tag.copy_from_slice(HELLO_TAG);
    hello.encode(fields);
    bytes
}

/// The bytes that the signature of a request or session open on `connection` covers:
/// the 20 bytes `foundation/request/1`, the connection (16), and `body`, the exact
/// bytes that the program sent.
#[must_use]
pub fn request(connection: connection::Key, body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(REQUEST_TAG.len() + 16 + body.len());
    bytes.extend_from_slice(REQUEST_TAG);
    bytes.extend_from_slice(&connection.0);
    bytes.extend_from_slice(body);
    bytes
}

/// A hello that [`Rules::admit`] took. Keep it for the connection, and give it to
/// [`Rules::verify`] with each request.
#[derive(Clone, Debug)]
pub struct Admitted {
    hello: Hello,
    ends: Stamp,
}

impl Admitted {
    /// The hello, with its subject for audit.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// When the hello stops being live at this node: the earlier of its `expires` and
    /// [`CAP`] past the latest mesh time at its admission. The owner closes the
    /// sessions of the connection at this stamp.
    #[must_use]
    pub fn ends(&self) -> Stamp {
        self.ends
    }
}

impl Rules {
    /// Checks `hello`, signed with `signature`, at mesh time `now` (`None` when the
    /// node has none). `peer` is the node that carried the hello: this node when the
    /// program connected to it, else the node whose transport session forwarded it.
    /// `admit` does not check `nonce`: the node that `via` names checks that it is the
    /// challenge that it sent (#1748).
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Unsynced`], [`Error::Unknown`],
    /// [`Error::Unlisted`], [`Error::Signature`], [`Error::Via`],
    /// [`Error::Expired`].
    pub fn admit(
        &self,
        now: Option<Interval>,
        peer: node::Key,
        hello: Hello,
        signature: &[u8; 64],
    ) -> Result<Admitted, Error> {
        let now = now.ok_or(Error::Unsynced)?;
        self.listed(&hello)?;
        hello
            .key
            .verify(&self::hello(&hello), signature)
            .map_err(|_bad| Error::Signature)?;
        if hello.via != peer {
            return Err(Error::Via {
                via: hello.via,
                peer,
            });
        }
        live(hello.expires, now)?;
        let ends = now
            .latest
            .checked_add(CAP)
            .map_or(hello.expires, |cap| hello.expires.min(cap));
        Ok(Admitted { hello, ends })
    }

    /// Checks `hello`, signed with `signature`, which renews `admitted` on its
    /// connection, as [`admit`](Self::admit) checks a first hello. Keep the result in
    /// place of `admitted`.
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Changed`] when `hello` names another
    /// value of a [`Field`] than `admitted`, then [`Error::Unsynced`],
    /// [`Error::Unknown`], [`Error::Unlisted`], [`Error::Signature`],
    /// [`Error::Expired`].
    pub fn renew(
        &self,
        admitted: &Admitted,
        now: Option<Interval>,
        hello: Hello,
        signature: &[u8; 64],
    ) -> Result<Admitted, Error> {
        let first = &admitted.hello;
        let changed = [
            (hello.subject != first.subject, Field::Subject),
            (hello.key != first.key, Field::Key),
            (hello.via != first.via, Field::Via),
            (hello.connection != first.connection, Field::Connection),
        ];
        if let Some(&(_, field)) = changed.iter().find(|(differs, _)| *differs) {
            return Err(Error::Changed { field });
        }
        self.admit(now, first.via, hello, signature)
    }

    /// Checks that `body`, signed with `signature`, is a request of the connection of
    /// `admitted`, at mesh time `now`: its subject still lists its key, the hello is
    /// live until [`Admitted::ends`], and the key signed [`request`] of the hello's
    /// connection and `body`.
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Unsynced`], [`Error::Unknown`],
    /// [`Error::Unlisted`], [`Error::Signature`], [`Error::Expired`].
    pub fn verify(
        &self,
        admitted: &Admitted,
        now: Option<Interval>,
        body: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), Error> {
        let now = now.ok_or(Error::Unsynced)?;
        let hello = &admitted.hello;
        self.listed(hello)?;
        hello
            .key
            .verify(&request(hello.connection, body), signature)
            .map_err(|_bad| Error::Signature)?;
        live(admitted.ends, now)
    }

    /// Refuses `hello` unless the spec lists its key for its subject.
    fn listed(&self, hello: &Hello) -> Result<(), Error> {
        let subject =
            self.subjects
                .get(&hello.subject)
                .ok_or_else(|| Error::Unknown {
                    subject: hello.subject.clone(),
                })?;
        subject
            .keys()
            .binary_search(&hello.key)
            .map(|_at| ())
            .map_err(|_at| Error::Unlisted {
                subject: hello.subject.clone(),
                key: hello.key,
            })
    }
}

/// Refuses a hello once the latest mesh time reaches `expires`.
fn live(expires: Stamp, now: Interval) -> Result<(), Error> {
    if now.latest >= expires {
        return Err(Error::Expired {
            expires,
            now: now.latest,
        });
    }
    Ok(())
}

/// Why a proof was refused. `Display` gives the message: a lower-case clause with no
/// final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The node has no mesh time yet.
    Unsynced,
    /// The spec has no subject of this name.
    Unknown {
        /// The subject of the hello.
        subject: Name,
    },
    /// The spec does not list the key for the subject.
    Unlisted {
        /// The subject of the hello.
        subject: Name,
        /// The key of the hello.
        key: PublicKey,
    },
    /// The signature is not of the message by the key.
    Signature,
    /// The hello names another node than the one that carried it.
    Via {
        /// The node that the hello names.
        via: node::Key,
        /// The node that carried the hello.
        peer: node::Key,
    },
    /// The hello ended at `expires`, at or before the latest mesh time `now`.
    Expired {
        /// When the hello stops being live at this node: the earlier of its `expires`
        /// and [`CAP`] past the latest mesh time at its admission ([`Admitted::ends`]).
        expires: Stamp,
        /// The latest the mesh time can be.
        now: Stamp,
    },
    /// A renewal names another value of `field` than the hello it renews: the first
    /// that differs, in the order of [`Field`].
    Changed {
        /// The field.
        field: Field,
    },
}

/// A field of a hello that a renewal must keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The subject.
    Subject,
    /// The key.
    Key,
    /// The node that the hello names as `via`.
    Via,
    /// The connection.
    Connection,
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Subject => "subject",
            Self::Key => "key",
            Self::Via => "`via` node",
            Self::Connection => "connection",
        })
    }
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Unsynced => "Connect again when the node has synced its clock",
            Self::Unknown { .. } => {
                "Define the subject with the program's public key in the spec"
            }
            Self::Unlisted { .. } => {
                "Add the key to the subject in the spec, or sign with a listed key"
            }
            Self::Signature => "Sign the exact bytes with the key of the hello",
            Self::Via { .. } => "Name the node that the program connects to as `via`",
            Self::Expired { .. } => {
                "Send a hello with a later expiry, and renew it before it ends"
            }
            Self::Changed { .. } => {
                "Renew with the subject, key, `via`, and connection of the hello it \
                 renews"
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsynced => f.write_str("the node has no mesh time yet"),
            Self::Unknown { subject } => write!(f, "the spec has no subject {subject}"),
            Self::Unlisted { subject, key } => {
                write!(f, "the spec does not list key {key} for subject {subject}")
            }
            Self::Signature => {
                f.write_str("the signature is not of the message by the key")
            }
            Self::Via { via, peer } => {
                write!(f, "the hello names node {via}, but node {peer} carried it")
            }
            Self::Expired { expires, now } => write!(
                f,
                "the hello ended at {expires}, at or before the mesh time {now}"
            ),
            Self::Changed { field } => {
                write!(
                    f,
                    "the renewal names another {field} than the hello it renews"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
