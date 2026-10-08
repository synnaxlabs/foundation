//! The proof of a subject: a signed hello, then signed requests on its connection.

use std::fmt;

use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::Name;
use types::node;
use types::time::{Span, Stamp};

/// How far past the earliest mesh time a hello may expire.
pub const CAP: Span = Span::from_nanos(15 * Span::MINUTE.nanos());

const HELLO_TAG: &[u8] = b"foundation hello 1\0";
const REQUEST_TAG: &[u8] = b"foundation request 1\0";

/// The bytes that the signature of `hello` covers. Each integer is little-endian:
/// the 18 bytes `foundation hello 1`, a zero byte, the length of the subject (1
/// byte), the subject, the key (32), `via` (16), the connection (16), the nonce
/// (16), and `expires` in nanoseconds (8).
///
/// # Panics
///
/// Never: a name is at most [`Name::MAX_BYTES`] (255) bytes.
#[must_use]
pub fn hello(hello: &Hello) -> Vec<u8> {
    let subject = hello.subject.as_str().as_bytes();
    let mut bytes = Vec::with_capacity(HELLO_TAG.len() + 1 + subject.len() + 88);
    bytes.extend_from_slice(HELLO_TAG);
    bytes.push(
        u8::try_from(subject.len()).expect("invariant: a name is at most 255 bytes"),
    );
    bytes.extend_from_slice(subject);
    bytes.extend_from_slice(&hello.key.to_bytes());
    bytes.extend_from_slice(&hello.via.as_u128().to_le_bytes());
    bytes.extend_from_slice(&hello.connection.0);
    bytes.extend_from_slice(&hello.nonce);
    bytes.extend_from_slice(&hello.expires.nanos().to_le_bytes());
    bytes
}

/// The bytes that the signature of a request or session open on `connection` covers:
/// the 20 bytes `foundation request 1`, a zero byte, the connection (16), and `body`,
/// the exact bytes that the program sent.
#[must_use]
pub fn request(connection: connection::Key, body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(REQUEST_TAG.len() + 16 + body.len());
    bytes.extend_from_slice(REQUEST_TAG);
    bytes.extend_from_slice(&connection.0);
    bytes.extend_from_slice(body);
    bytes
}

/// A hello that [`Rules::admit`](crate::Rules::admit) took. Keep it for the
/// connection, and give it to [`Rules::verify`](crate::Rules::verify) with each
/// request.
#[derive(Clone, Debug)]
pub struct Admitted {
    pub(crate) hello: Hello,
}

impl Admitted {
    /// The hello: its subject for audit, and `expires`, at which the owner closes the
    /// sessions of the connection.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.hello
    }
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
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
