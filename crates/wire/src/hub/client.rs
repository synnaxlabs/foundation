//! The hub streams of a program's session with the node it connects to.
//!
//! - The hello stream is the first hub stream of the session and lives as long as it.
//!   The node sends a [`Challenge`] first and again after each hello that it admits.
//!   The program sends a [`Signed`] hello that echoes the last challenge, first and
//!   then to renew.
//! - A request stream carries one request. The program sends a [`Request`] and then
//!   its body, and the node sends a [`Response`] and then its body.
//! - A read stream carries one reader session. The program sends a [`Read`] and then
//!   its [`Open`] as a body. The node sends [`Places`] and the run of its [`place`]s,
//!   and then the replies of a reader session, from `Opened`. The program sends no
//!   credit. It decodes the places and the run with [`Reader`], which then gives the
//!   [`super::Reader`] of the replies.
//! - A write stream carries one writer session. The program sends a [`write::Write`]
//!   and then its [`write::Open`] as a body. The node sends [`Places`] and the run of
//!   its [`place`]s. The program decodes them with [`write::Opening`]. Then the
//!   program sends each frame as a [`super::write::Head`], the run of its
//!   [`ends`](super::ends), and its body, and the node sends a
//!   [`super::write::Reply`] for each frame and each commit.
//!
//! A body goes as a run of stream messages back to back, with no prefix, each at most
//! the peer's `message_bytes_max` and none empty. It holds exactly the length of its
//! request, response, read, or write, so the receiver counts it to find where it
//! ends. Only the frames of a write follow a body.
//!
//! The program knows the kind of each stream, so it calls the `decode` of the message
//! that it expects, or of its [`Reader`] on a read stream. The node knows the kind of
//! the hello stream only: it decodes the first message of each later stream with
//! [`First::decode`]. Each side counts a body with the [`Body`] of its request,
//! response, read, or write.
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
//! - [`Read`]: kind 6, the open's length (`u64`), and the signature (64).
//! - [`Open`]: the mode (`u8`, latest 1, complete 2), then each pattern of the selector
//!   as written, an exclusion with its leading `!`: its length (`u16`) and its text.
//! - [`Places`]: kind 6, the count of places (`u32`), and the place of the index
//!   (`u32`).
//! - [`place`]: each place's key (`u128`), its sample type as text (length `u8`, then
//!   the text), and its name (length `u8`, then the name).
//! - [`write::Write`]: kind 7, the open's length (`u64`), and the signature (64).
//! - [`write::Open`]: the authority (`u8`), the lease (`u8`, none 0, a lease 1, then
//!   the lease as `i64` nanoseconds), then each channel's name: its length (`u8`) and
//!   the name.
//!
//! The formats of a write's frames and of the node's replies are in
//! [`super::write`].

use std::fmt;

use types::channel;
use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::name::{self, Name};
use types::node;
use types::sample;
use types::time::{Interval, Stamp};

use super::{BUSY, FAILED, NOT_HOME, UNKNOWN};
use crate::common::{Fields, Length, Run, Writer, body};
use crate::header::MALFORMED;

mod read;
mod stream;
pub mod text;
pub mod write;

pub use read::{Mode, Open, Read, Reader, Start};
pub use stream::{OPEN_BYTES_MAX, PLACES_MAX, Place, Places, place};

const HELLO: u8 = 4;
const REQUEST: u8 = 5;
const READ: u8 = 6;
const WRITE: u8 = 7;

/// Stop code: the hello, request, read, or write failed the access check. The spec has
/// no such subject, does not list the key for it, or the signature is not valid. The
/// node's log names which.
pub const REFUSED: u32 = 20;

/// Stop code: the node has no mesh time yet.
pub const UNSYNCED: u32 = 21;

/// Stop code: the hello does not echo the nonce of the node's last challenge.
pub const STALE: u32 = 22;

/// Stop code: the hello names another node as `via` than the node that carried it.
pub const VIA: u32 = 23;

/// Stop code: the hello ended, at its `expires` or earlier, at the cap of the node on
/// the life of a hello.
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
    /// [`UNKNOWN`]: the selector of a read matches no channel, a channel of the read
    /// was removed, or a channel of a write is unknown or was removed.
    Unknown,
    /// [`NOT_HOME`]: the home of the index of a read is another node, at the open or
    /// after a move, or the home of the index of a write is another node at the open.
    NotHome,
    /// [`FAILED`]: the reader of a read failed, or the writer of a write failed or
    /// could not store a frame.
    Failed,
    /// [`BUSY`]: the node had no memory for a response, a request body, a read, the
    /// open or the ends of a write, or a reply of a write.
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
    const ALL: [Self; 11] = [
        Self::Malformed,
        Self::Unknown,
        Self::NotHome,
        Self::Failed,
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
            Self::Unknown => UNKNOWN,
            Self::NotHome => NOT_HOME,
            Self::Failed => FAILED,
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
            Self::Unknown => {
                "the selector matches no channel, or a channel of the read or write is \
                 unknown or was removed"
            }
            Self::NotHome => {
                "the home of the index of the read or write is another node"
            }
            Self::Failed => {
                "the reader or writer failed: the buffer or the mesh of the home of the \
                 index stopped, or the path from the node to that home failed"
            }
            Self::Busy => {
                "the node had no memory for a response, a request body, a read, or a \
                 write"
            }
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
            Self::Expired => "the hello ended",
            Self::Changed => {
                "a renewal names another subject, key, via, or connection than \
                 the hello it renews"
            }
        })
    }
}

/// A message of the client wire that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The message has no bytes.
    Empty,
    /// The first byte names no message of this stream.
    Kind {
        /// The first byte.
        kind: u8,
    },
    /// No message of its kind has this length.
    Length {
        /// The bytes of the message.
        len: usize,
    },
    /// The subject of a hello is not a name.
    Subject(text::Error<Box<name::Error>>),
    /// The key of a hello is a point of small order.
    SmallOrder,
    /// A message comes after the body of a request, a response, or a read.
    Trailing,
    /// A request or a response has a body over [`BODY_BYTES_MAX`], or a read or a
    /// write has an open over [`OPEN_BYTES_MAX`].
    Oversize {
        /// The bytes of the body.
        length: u64,
        /// The cap of the body.
        cap: u64,
    },
    /// A message of a body has more bytes than remain in the body.
    Body {
        /// The bytes of the message.
        len: usize,
        /// The bytes that remain in the body.
        remain: usize,
    },
    /// The stream ended before its body.
    Unfinished {
        /// The bytes of the body that did not come.
        remain: usize,
    },
    /// The open of a read names no mode.
    Mode {
        /// The mode byte.
        byte: u8,
    },
    /// The open of a read has a pattern that does not read, or none that includes
    /// names.
    Selector(text::Error<Box<name::Error>>),
    /// The places of a read or a write are 0 or over [`PLACES_MAX`].
    Places {
        /// The count of places.
        count: u32,
        /// The cap.
        cap: u32,
    },
    /// A place names an index that is not a place whose index is itself.
    Index {
        /// The place.
        place: u32,
        /// The index that it names.
        index: u32,
    },
    /// A place of a read names an index other than that of place 0: a read has one
    /// index.
    Indexes {
        /// The place.
        place: u32,
        /// The index that it names.
        index: u32,
    },
    /// The sample type of a place does not read.
    Sample(text::Error<sample::Error>),
    /// The name of a place does not read.
    Name(text::Error<Box<name::Error>>),
    /// A message of the run of places has more places than remain in the run.
    Run {
        /// The places of the message.
        items: usize,
        /// The places that remain in the run.
        remain: u32,
    },
    /// An index place has a sample type that is not `Stamp`.
    Unstamped {
        /// The index place.
        place: u32,
        /// The sample type of the index place.
        sample: sample::Type,
    },
    /// The places name a channel more than once.
    Repeated {
        /// The channel.
        key: channel::Key,
    },
    /// The node finished a read or a write stream before its last place: the
    /// `decode` of its [`Reader`] or [`write::Opening`] got `None`.
    Finished,
    /// The open of a write marks its lease with a byte that is neither 0 nor 1.
    Lease {
        /// The lease byte.
        byte: u8,
    },
    /// A channel name of the open of a write does not read.
    Channel(text::Error<Box<name::Error>>),
    /// The open of a write names no channel.
    Unnamed,
}

impl fmt::Display for Error {
    #[expect(clippy::too_many_lines, reason = "one arm for each error")]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the hub message is empty"),
            Self::Kind { kind } => write!(
                f,
                "the hub message has kind {kind}, which this stream does not carry"
            ),
            Self::Length { len } => write!(
                f,
                "the hub message has {len} bytes, which no message of its kind has"
            ),
            Self::Subject(cause) => {
                write!(f, "the subject of the hello is not a name: {cause}")
            }
            Self::SmallOrder => {
                f.write_str("the key of the hello is a point of small order")
            }
            Self::Trailing => {
                f.write_str("a hub message came after the body of the stream")
            }
            Self::Oversize { length, cap } => {
                write!(f, "the body has {length} bytes, over the cap of {cap}")
            }
            Self::Body { len, remain } => write!(
                f,
                "the body message has {len} bytes, and {remain} remain in the body"
            ),
            Self::Unfinished { remain } => write!(
                f,
                "the stream ended with {remain} bytes of its body to come"
            ),
            Self::Mode { byte } => {
                write!(
                    f,
                    "the open of the read names mode {byte}, which no read has"
                )
            }
            Self::Selector(cause) => {
                write!(f, "the selector of the read does not read: {cause}")
            }
            Self::Places { count, cap } => {
                write!(
                    f,
                    "the stream has {count} places, not from 1 to the cap of {cap}"
                )
            }
            Self::Index { place, index } => write!(
                f,
                "place {place} names place {index} as its index, which is not an index"
            ),
            Self::Indexes { place, index } => write!(
                f,
                "place {place} names place {index} as its index, and a read has the one \
                 index of place 0"
            ),
            Self::Sample(cause) => {
                write!(f, "the sample type of a place does not read: {cause}")
            }
            Self::Name(cause) => {
                write!(f, "the name of a place does not read: {cause}")
            }
            Self::Run { items, remain } => write!(
                f,
                "the run message holds {items} items, and {remain} remain in the run"
            ),
            Self::Unstamped { place, sample } => write!(
                f,
                "the index place {place} has sample type {sample}, not timestamp"
            ),
            Self::Repeated { key } => {
                write!(f, "the places name channel {key} more than once")
            }
            Self::Finished => {
                f.write_str("the node finished the stream before its last place")
            }
            Self::Lease { byte } => write!(
                f,
                "the open of the write marks its lease with {byte}, not 0 or 1"
            ),
            Self::Channel(cause) => write!(
                f,
                "a channel name of the open of the write does not read: {cause}"
            ),
            Self::Unnamed => f.write_str("the open of the write names no channel"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Subject(cause)
            | Self::Selector(cause)
            | Self::Name(cause)
            | Self::Channel(cause) => Some(cause),
            Self::Sample(cause) => Some(cause),
            _ => None,
        }
    }
}

impl From<Length> for Error {
    fn from(Length { len }: Length) -> Self {
        Self::Length { len }
    }
}

impl From<Run> for Error {
    fn from(Run { items, remain }: Run) -> Self {
        Self::Run { items, remain }
    }
}

impl From<body::Error> for Error {
    fn from(error: body::Error) -> Self {
        match error {
            body::Error::Empty => Self::Empty,
            body::Error::Over { len, remain } => Self::Body { len, remain },
            body::Error::Unfinished { remain } => Self::Unfinished { remain },
        }
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
        let nonce = *fields.take()?;
        let earliest = stamp(*fields.take()?);
        let latest = stamp(*fields.take()?);
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
        let [len] = *fields.take()?;
        let subject = fields.take_slice(usize::from(len))?;
        let key = *fields.take()?;
        let via = u128::from_le_bytes(*fields.take()?);
        let connection = *fields.take()?;
        let nonce = *fields.take()?;
        let expires = stamp(*fields.take()?);
        let signature = *fields.take()?;
        fields.end()?;
        let subject: Name = text::read(subject).map_err(Error::Subject)?;
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
        let length = *fields.take()?;
        let signature = *fields.take()?;
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

/// The first message of a stream after the hello stream, which gives the stream's
/// kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum First {
    /// A request stream.
    Request(Request),
    /// A read stream.
    Read(Read),
    /// A write stream.
    Write(write::Write),
}

impl First {
    /// Decodes the first message of a stream after the hello stream.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 5, 6, or 7,
    /// and the errors of [`Request::decode`], [`Read::decode`], and
    /// [`write::Write::decode`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        match message.first() {
            Some(&READ) => Read::decode(message).map(Self::Read),
            Some(&WRITE) => write::Write::decode(message).map(Self::Write),
            _ => Request::decode(message).map(Self::Request),
        }
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
        let length = *fields.take()?;
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

/// The rest of the body of one request, response, read, or write.
#[derive(Debug)]
pub struct Body(body::Count);

impl Body {
    fn new(length: u64) -> Self {
        assert_body(length);
        let remain = usize::try_from(length)
            .expect("invariant: a usize holds a body of at most 16 MiB");
        Self(body::Count::new(remain))
    }

    /// Takes the next message of the body and gives its bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] for an empty message, [`Error::Body`] for more bytes than
    /// remain, and [`Error::Trailing`] once the body ended.
    pub fn take<'m>(&mut self, message: &'m [u8]) -> Result<&'m [u8], Error> {
        if self.0.remain() == 0 {
            return Err(Error::Trailing);
        }
        self.0.take(message.len())?;
        Ok(message)
    }

    /// The bytes that remain. The body ended at 0.
    #[must_use]
    pub fn remain(&self) -> usize {
        self.0.remain()
    }

    /// Checks that the body ended, when its stream ends.
    ///
    /// # Errors
    ///
    /// [`Error::Unfinished`] when bytes of the body remain.
    pub fn end(&self) -> Result<(), Error> {
        Ok(self.0.end()?)
    }
}

/// The fields of `message` after its kind byte, which must be `kind`.
fn fields(message: &[u8], kind: u8) -> Result<Fields<'_>, Error> {
    let mut fields = Fields::new(message);
    match fields.kind() {
        None => Err(Error::Empty),
        Some(first) if first == kind => Ok(fields),
        Some(kind) => Err(Error::Kind { kind }),
    }
}

fn stamp(word: [u8; 8]) -> Stamp {
    Stamp::from_nanos(i64::from_le_bytes(word))
}

/// The body length in `word`.
fn body(word: [u8; 8]) -> Result<u64, Error> {
    let length = u64::from_le_bytes(word);
    if length > BODY_BYTES_MAX {
        return Err(Error::Oversize {
            length,
            cap: BODY_BYTES_MAX,
        });
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
