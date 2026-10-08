//! The proof of a subject: a signed hello, then signed requests on its connection.

use std::fmt;

use spec::definition::Kind;
use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::Name;
use types::node;
use types::time::{Interval, Span, Stamp};

use crate::Rules;

/// How far past the earliest mesh time a hello may expire.
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
}

impl Admitted {
    /// The hello: its subject for audit, and `expires`, at which the owner closes the
    /// sessions of the connection.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.hello
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
    /// [`Error::Expired`], [`Error::Capped`].
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
        live(&hello, now)?;
        if let Some(cap) = now.earliest.checked_add(CAP)
            && hello.expires > cap
        {
            return Err(Error::Capped {
                expires: hello.expires,
                cap,
            });
        }
        Ok(Admitted { hello })
    }

    /// Checks `hello`, signed with `signature`, which renews `admitted` on its
    /// connection, as [`admit`](Self::admit) checks a first hello. Keep the result in
    /// place of `admitted`.
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Changed`] when `hello` names another
    /// subject, key, `via`, or connection than `admitted`, then [`Error::Unsynced`],
    /// [`Error::Unknown`], [`Error::Unlisted`], [`Error::Signature`],
    /// [`Error::Expired`], [`Error::Capped`].
    pub fn renew(
        &self,
        admitted: &Admitted,
        now: Option<Interval>,
        hello: Hello,
        signature: &[u8; 64],
    ) -> Result<Admitted, Error> {
        let first = &admitted.hello;
        if hello.subject != first.subject
            || hello.key != first.key
            || hello.via != first.via
            || hello.connection != first.connection
        {
            return Err(Error::Changed);
        }
        self.admit(now, first.via, hello, signature)
    }

    /// Checks that `body`, signed with `signature`, is a request of the connection of
    /// `admitted`, at mesh time `now`: its subject still lists its key, the hello has
    /// not expired, and the key signed [`request`] of the hello's connection and
    /// `body`.
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
        live(hello, now)
    }

    /// Refuses `hello` unless the spec lists its key for its subject. A subject that
    /// makes no tree key has no definition.
    fn listed(&self, hello: &Hello) -> Result<(), Error> {
        let subject = Kind::Subject
            .key(hello.subject.as_str())
            .ok()
            .and_then(|key| self.subjects.get(&key))
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

/// Refuses `hello` once the latest mesh time reaches its expiry.
fn live(hello: &Hello, now: Interval) -> Result<(), Error> {
    if now.latest >= hello.expires {
        return Err(Error::Expired {
            expires: hello.expires,
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
    /// The hello expired at `expires`, at or before the latest mesh time `now`.
    Expired {
        /// When the hello expires.
        expires: Stamp,
        /// The latest the mesh time can be.
        now: Stamp,
    },
    /// The hello expires later than [`CAP`] past the earliest mesh time.
    Capped {
        /// When the hello expires.
        expires: Stamp,
        /// The latest expiry that the node takes: [`CAP`] past the earliest mesh time.
        cap: Stamp,
    },
    /// A renewal names another subject, key, `via`, or connection than the hello it
    /// renews.
    Changed,
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
            Self::Expired { .. } => "Send a new hello with a later expiry",
            Self::Capped { .. } => "Send a hello that expires within 15 minutes",
            Self::Changed => {
                "Renew with the subject, key, `via`, and connection of the hello it renews"
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
                "the hello expired at {expires}, at or before the mesh time {now}"
            ),
            Self::Capped { expires, cap } => {
                write!(f, "the hello expires at {expires}, after the cap {cap}")
            }
            Self::Changed => f.write_str(
                "the renewal names another subject, key, node, or connection than \
                 the hello it renews",
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
