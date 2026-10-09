//! The hub streams of a program's session with the node it connects to.
//!
//! - The hello stream is the first hub stream of the session and lives as long as it.
//!   The node sends a [`Challenge`] first and again after each hello that it admits.
//!   The program sends a [`Signed`] hello that echoes the last challenge, first and
//!   then to renew.
//! - A request stream carries one request. The program sends a [`Request`] and then
//!   its body, and the node sends a [`Response`] and then its body.
//!
//! A body goes as a run of stream messages back to back, with no prefix, each at most
//! the peer's `message_bytes_max` and none empty. It holds exactly the length of its
//! request or response, so the receiver counts it to find where it ends. No message
//! follows a body.
//!
//! Each side knows the kind of each stream, so it calls the `decode` of the message
//! that it expects, and counts a body with the [`Body`] of its request or response.
//!
//! Fields are little-endian.
//!
//! - [`Challenge`]: kind 4, the nonce (16), then the earliest and the latest mesh time
//!   (`i64` nanoseconds each).
//! - [`Signed`]: kind 4, the hello as [`Hello::encode`] writes it (the subject's
//!   length (`u8`) and bytes, the key (32), `via` (`u128`), the connection (16), the
//!   nonce (16), `expires` (`i64` nanoseconds)), and the signature (64). These are the
//!   signed bytes of the hello with no tag.
//! - [`Request`]: kind 5, the body's length (`u64`), and the signature (64).
//! - [`Response`]: kind 5 and the body's length (`u64`).

use std::fmt;

use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::Name;
use types::node;
use types::time::{Interval, Stamp};

use super::{BUSY, Error};
use crate::common::{Fields, Writer};
use crate::header::MALFORMED;

const HELLO: u8 = 4;
const REQUEST: u8 = 5;

/// Stop code: the hello or request failed the access check. The spec has no such
/// subject, does not list the key for it, or the signature is not valid. The node's
/// log names which.
pub const REFUSED: u32 = 20;

/// Stop code: the node has no mesh time yet.
pub const UNSYNCED: u32 = 21;

/// Stop code: the hello does not echo the nonce of the node's last challenge.
pub const STALE: u32 = 22;

/// Stop code: the hello names another node as `via` than the node that carried it.
pub const VIA: u32 = 23;

/// Stop code: the hello expired.
pub const EXPIRED: u32 = 24;

/// Stop code: a renewal names another subject, key, `via`, or connection than the
/// hello it renews.
pub const CHANGED: u32 = 26;

/// A code that a node stops a client stream or closes a client session with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// [`MALFORMED`]: a message of the program does not decode, or breaks a rule of
    /// the client wire.
    Malformed,
    /// [`BUSY`]: the node had no memory for a response.
    Busy,
    /// [`REFUSED`].
    Refused,
    /// [`UNSYNCED`].
    Unsynced,
    /// [`STALE`].
    Stale,
    /// [`VIA`].
    Via,
    /// [`EXPIRED`].
    Expired,
    /// [`CHANGED`].
    Changed,
}

impl Refusal {
    const ALL: [Self; 8] = [
        Self::Malformed,
        Self::Busy,
        Self::Refused,
        Self::Unsynced,
        Self::Stale,
        Self::Via,
        Self::Expired,
        Self::Changed,
    ];

    /// The refusal of `code`, or `None` for 0 or a code outside the set.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|refusal| refusal.code() == code)
    }

    /// The code on the wire.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Malformed => MALFORMED,
            Self::Busy => BUSY,
            Self::Refused => REFUSED,
            Self::Unsynced => UNSYNCED,
            Self::Stale => STALE,
            Self::Via => VIA,
            Self::Expired => EXPIRED,
            Self::Changed => CHANGED,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "a message of the program broke the client wire",
            Self::Busy => "the node had no memory for a response",
            Self::Refused => {
                "the spec has no such subject, does not list the key for it, or the \
                 signature is not valid"
            }
            Self::Unsynced => "the node has no mesh time yet",
            Self::Stale => {
                "the hello does not echo the nonce of the node's last \
                 challenge"
            }
            Self::Via => "the hello names another node as via",
            Self::Expired => "the hello expired",
            Self::Changed => {
                "a renewal names another subject, key, via, or connection than \
                 the hello it renews"
            }
        })
    }
}

/// The most bytes that the body of a request or a response holds: 16 MiB.
pub const BODY_BYTES_MAX: u64 = 16 << 20;

/// What the node sends first on the hello stream, and again after each hello that it
/// admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Challenge {
    /// Random bytes that the next hello echoes.
    pub nonce: [u8; 16],
    /// The node's mesh time when it sent the challenge, from which the program sets
    /// the expiry of the next hello.
    pub now: Interval,
}

impl Challenge {
    /// The bytes of an encoded challenge.
    pub const LEN: usize = 33;

    /// Writes the challenge into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Challenge::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[HELLO]);
        out.put(&self.nonce);
        out.put(&self.now.earliest.nanos().to_le_bytes());
        out.put(&self.now.latest.nanos().to_le_bytes());
    }

    /// Decodes a challenge.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 4, and
    /// [`Error::Length`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(message, HELLO)?;
        let nonce = fields.take()?;
        let earliest = stamp(fields.take()?);
        let latest = stamp(fields.take()?);
        fields.end()?;
        Ok(Self {
            nonce,
            now: Interval { earliest, latest },
        })
    }
}

/// A hello and the signature of its subject's key over its signed bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    /// The hello.
    pub hello: Hello,
    /// The signature of the hello.
    pub signature: [u8; 64],
}

impl Signed {
    /// The bytes of the encoded hello.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.hello.encoded_len().saturating_add(65)
    }

    /// Writes the hello into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Signed::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        out.put(&[HELLO]);
        self.hello.encode(out.field(self.hello.encoded_len()));
        out.put(&self.signature);
    }

    /// Decodes a hello.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 4,
    /// [`Error::Length`], [`Error::Subject`], and then [`Error::SmallOrder`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(message, HELLO)?;
        let [len] = fields.take()?;
        let subject = fields.take_slice(usize::from(len))?;
        let key = fields.take()?;
        let via = u128::from_le_bytes(fields.take()?);
        let connection = fields.take()?;
        let nonce = fields.take()?;
        let expires = stamp(fields.take()?);
        let signature = fields.take()?;
        fields.end()?;
        let subject = std::str::from_utf8(subject)
            .ok()
            .and_then(|subject| subject.parse::<Name>().ok())
            .ok_or(Error::Subject)?;
        let key = PublicKey::new(key).map_err(|_small| Error::SmallOrder)?;
        Ok(Self {
            hello: Hello {
                subject,
                key,
                via: node::Key::from_u128(via),
                connection: connection::Key(connection),
                nonce,
                expires,
            },
            signature,
        })
    }
}

/// The fixed part of a request. Its body follows, unless `length` is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The bytes of the body, at most [`BODY_BYTES_MAX`].
    pub length: u64,
    /// The signature of the hello's key over the signed bytes of the request: the
    /// connection and the body.
    pub signature: [u8; 64],
}

impl Request {
    /// The bytes of an encoded request.
    pub const LEN: usize = 73;

    /// Writes the request into `out`.
    ///
    /// # Panics
    ///
    /// When `length` is over [`BODY_BYTES_MAX`], or `out` is not [`Request::LEN`]
    /// bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert_body(self.length);
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[REQUEST]);
        out.put(&self.length.to_le_bytes());
        out.put(&self.signature);
    }

    /// Decodes a request.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 5,
    /// [`Error::Length`], and [`Error::Oversize`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(message, REQUEST)?;
        let length = fields.take()?;
        let signature = fields.take()?;
        fields.end()?;
        Ok(Self {
            length: body(length)?,
            signature,
        })
    }

    /// The body that follows this request.
    ///
    /// # Panics
    ///
    /// When `length` is over [`BODY_BYTES_MAX`].
    #[must_use]
    pub fn body(&self) -> Body {
        Body::new(self.length)
    }
}

/// The fixed part of a response. Its body follows, unless `length` is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    /// The bytes of the body, at most [`BODY_BYTES_MAX`].
    pub length: u64,
}

impl Response {
    /// The bytes of an encoded response.
    pub const LEN: usize = 9;

    /// Writes the response into `out`.
    ///
    /// # Panics
    ///
    /// When `length` is over [`BODY_BYTES_MAX`], or `out` is not [`Response::LEN`]
    /// bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert_body(self.length);
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[REQUEST]);
        out.put(&self.length.to_le_bytes());
    }

    /// Decodes a response.
    ///
    /// # Errors
    ///
    /// As [`Request::decode`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(message, REQUEST)?;
        let length = fields.take()?;
        fields.end()?;
        Ok(Self {
            length: body(length)?,
        })
    }

    /// The body that follows this response.
    ///
    /// # Panics
    ///
    /// When `length` is over [`BODY_BYTES_MAX`].
    #[must_use]
    pub fn body(&self) -> Body {
        Body::new(self.length)
    }
}

/// The rest of the body of one request or response.
#[derive(Debug)]
pub struct Body {
    remain: usize,
}

impl Body {
    fn new(length: u64) -> Self {
        assert_body(length);
        let remain = usize::try_from(length)
            .expect("invariant: a usize holds a body of at most 16 MiB");
        Self { remain }
    }

    /// Takes the next message of the body and gives its bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] for an empty message, [`Error::Body`] for more bytes than
    /// remain, and [`Error::Trailing`] once the body ended.
    pub fn take<'m>(&mut self, message: &'m [u8]) -> Result<&'m [u8], Error> {
        let (len, remain) = (message.len(), self.remain);
        if remain == 0 {
            return Err(Error::Trailing);
        }
        if len == 0 {
            return Err(Error::Empty);
        }
        self.remain = remain.checked_sub(len).ok_or(Error::Body { len, remain })?;
        Ok(message)
    }

    /// The bytes that remain. The body ended at 0.
    #[must_use]
    pub fn remain(&self) -> usize {
        self.remain
    }

    /// Checks that the body ended, when its stream ends.
    ///
    /// # Errors
    ///
    /// [`Error::Unfinished`] when bytes of the body remain.
    pub fn end(&self) -> Result<(), Error> {
        match self.remain {
            0 => Ok(()),
            remain => Err(Error::Unfinished { remain }),
        }
    }
}

/// The fields of `message` after its kind byte, which must be `kind`.
fn fields(message: &[u8], kind: u8) -> Result<Fields<'_, Error>, Error> {
    match message.split_first() {
        None => Err(Error::Empty),
        Some((&first, rest)) if first == kind => {
            Ok(Fields::new(rest, Error::Length { len: message.len() }))
        }
        Some((&kind, _rest)) => Err(Error::Kind { kind }),
    }
}

fn stamp(word: [u8; 8]) -> Stamp {
    Stamp::from_nanos(i64::from_le_bytes(word))
}

/// The body length in `word`.
fn body(word: [u8; 8]) -> Result<u64, Error> {
    let length = u64::from_le_bytes(word);
    if length > BODY_BYTES_MAX {
        return Err(Error::Oversize { length });
    }
    Ok(length)
}

fn assert_body(length: u64) {
    assert!(
        length <= BODY_BYTES_MAX,
        "a body of {length} bytes is over the cap of {BODY_BYTES_MAX}"
    );
}

#[cfg(test)]
mod tests;
