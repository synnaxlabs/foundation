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
//! [`Gateway`] decodes the messages from the program, and [`Program`] those from the
//! node. Each takes the kind of the first message as the kind of the stream, and
//! checks the order and the body of that stream.
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

use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::Name;
use types::node;
use types::time::{Interval, Stamp};

use super::Error;
use crate::common::{Fields, Writer};

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

/// Stop code: the hello expires later than the cap past the earliest mesh time.
pub const CAPPED: u32 = 25;

/// Stop code: a renewal names another subject, key, `via`, or connection than the
/// hello it renews.
pub const CHANGED: u32 = 26;

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

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(bytes);
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

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(bytes);
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

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(bytes);
        let length = body(fields.take()?)?;
        let signature = fields.take()?;
        fields.end()?;
        Ok(Self { length, signature })
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

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(bytes);
        let length = body(fields.take()?)?;
        fields.end()?;
        Ok(Self { length })
    }
}

/// The decoder at the node: it takes each message from the program on one hub
/// stream, in order, and checks the order and the body of the stream.
#[derive(Debug, Default)]
pub struct Gateway {
    order: Order,
}

/// A message from the program, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromProgram<'m> {
    /// A hello. The stream is the hello stream.
    Signed(Signed),
    /// A request. The stream is a request stream. Its body follows as `length` bytes
    /// of `Body` messages; with `length` 0, none follows and the stream is complete.
    Request(Request),
    /// One message of the request's body.
    Body {
        /// The bytes of the message.
        bytes: &'m [u8],
        /// The body ends with this message.
        last: bool,
    },
}

impl Gateway {
    /// Decodes the next message from the program.
    ///
    /// # Errors
    ///
    /// The [`Error`] of a message that does not decode, or that breaks the order or
    /// the body of the stream: [`Error::Mixed`] for a request on the hello stream,
    /// [`Error::Body`] for a message longer than the rest of the body, and
    /// [`Error::Trailing`] for a message after the body. A message of a body has no
    /// kind, so a message where the body continues is read as body bytes. The stream
    /// is then not valid ([`MALFORMED`](crate::header::MALFORMED)), and the caller
    /// stops it.
    pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<FromProgram<'m>, Error> {
        if let Some((bytes, last)) = self.order.body(message)? {
            return Ok(FromProgram::Body { bytes, last });
        }
        let (decoded, length) = match kind(message)? {
            HELLO => (FromProgram::Signed(Signed::decode(message)?), None),
            REQUEST => {
                let request = Request::decode(message)?;
                (FromProgram::Request(request), Some(request.length))
            }
            kind => return Err(Error::Kind { kind }),
        };
        self.order.next(length)?;
        Ok(decoded)
    }
}

/// The decoder at the program: it takes each message from the node on one hub
/// stream, in order, and checks the order and the body of the stream.
#[derive(Debug, Default)]
pub struct Program {
    order: Order,
}

/// A message from the node, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FromGateway<'m> {
    /// A challenge. The stream is the hello stream.
    Challenge(Challenge),
    /// A response. The stream is a request stream. Its body follows as `length` bytes
    /// of `Body` messages; with `length` 0, none follows and the stream is complete.
    Response(Response),
    /// One message of the response's body.
    Body {
        /// The bytes of the message.
        bytes: &'m [u8],
        /// The body ends with this message.
        last: bool,
    },
}

impl Program {
    /// Decodes the next message from the node.
    ///
    /// # Errors
    ///
    /// As [`Gateway::decode`], for a challenge in place of a hello and a response in
    /// place of a request.
    pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<FromGateway<'m>, Error> {
        if let Some((bytes, last)) = self.order.body(message)? {
            return Ok(FromGateway::Body { bytes, last });
        }
        let (decoded, length) = match kind(message)? {
            HELLO => (FromGateway::Challenge(Challenge::decode(message)?), None),
            REQUEST => {
                let response = Response::decode(message)?;
                (FromGateway::Response(response), Some(response.length))
            }
            kind => return Err(Error::Kind { kind }),
        };
        self.order.next(length)?;
        Ok(decoded)
    }
}

/// The order of one client stream, the same on both sides.
#[derive(Clone, Copy, Debug, Default)]
enum Order {
    /// No message yet: the first message sets the kind of the stream.
    #[default]
    First,
    /// The hello stream, which takes only messages of kind 4.
    Hello,
    /// The body of a request stream, with the bytes that remain.
    Body { remain: usize },
    /// The request stream after its body, which takes no message.
    Done,
}

impl Order {
    /// Takes `message` as body bytes, with whether the body ends with it, where a body
    /// continues. Gives `None` where the next message has a kind.
    fn body<'m>(
        &mut self,
        message: &'m [u8],
    ) -> Result<Option<(&'m [u8], bool)>, Error> {
        match *self {
            Self::First | Self::Hello => Ok(None),
            Self::Done => Err(Error::Trailing),
            Self::Body { remain } => {
                let len = message.len();
                if len == 0 {
                    return Err(Error::Empty);
                }
                let remain =
                    remain.checked_sub(len).ok_or(Error::Body { len, remain })?;
                *self = if remain == 0 {
                    Self::Done
                } else {
                    Self::Body { remain }
                };
                Ok(Some((message, remain == 0)))
            }
        }
    }

    /// Takes a decoded message after [`body`](Self::body) gave `None`: `None` for a
    /// message of the hello stream, else the body length of a request or response.
    fn next(&mut self, length: Option<u64>) -> Result<(), Error> {
        *self = match (*self, length) {
            (_, None) => Self::Hello,
            (Self::First, Some(0)) => Self::Done,
            (Self::First, Some(length)) => Self::Body {
                remain: body_len(length),
            },
            (_, Some(_)) => return Err(Error::Mixed { kind: REQUEST }),
        };
        Ok(())
    }
}

/// The kind byte of `message`.
fn kind(message: &[u8]) -> Result<u8, Error> {
    message.first().copied().ok_or(Error::Empty)
}

/// The fields of `bytes` after its kind byte.
fn fields(bytes: &[u8]) -> Fields<'_, Error> {
    Fields::new(
        bytes.get(1..).unwrap_or_default(),
        Error::Length { len: bytes.len() },
    )
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

fn body_len(length: u64) -> usize {
    usize::try_from(length).expect("invariant: a usize holds a body of at most 16 MiB")
}

fn assert_body(length: u64) {
    assert!(
        length <= BODY_BYTES_MAX,
        "a body of {length} bytes is over the cap of {BODY_BYTES_MAX}"
    );
}

#[cfg(test)]
mod tests;
