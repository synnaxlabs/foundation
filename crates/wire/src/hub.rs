//! A remote reader session: one hub stream from the reader's node to the home of an
//! index. After the header, the reader's node sends an [`Open`] and then
//! [`Credit`] messages, and the home sends [`Reply`] messages.
//!
//! A run follows some messages: the keys of an open, and the ends and then the body
//! of a head. A run goes as stream messages back to back, with no prefix, each at most
//! the peer's `message_bytes_max`. A message of a run never splits a key or an end,
//! and no message is empty. The keys run holds exactly [`Open::channels`] keys, and the
//! ends run exactly [`Head::series`] ends, so the receiver counts them to find where a
//! run ends. The body starts a new message.
//!
//! The streams of a program's session are in [`client`], the frames of its write
//! stream and their replies in [`write`](mod@write), and the stream on which its node
//! carries its write to the home in [`forward`].
//!
//! [`Home`] decodes the messages from the reader's node, and [`Reader`] those from the
//! home. Each checks the order and the runs of its side of the session.
//!
//! A message that does not decode, comes from the wrong side, or breaks a rule of this
//! module stops the stream with [`MALFORMED`](crate::header::MALFORMED).
//!
//! Fields are little-endian.
//!
//! - [`Open`]: kind 1 (latest) or 2 (complete), then for complete `limit_bytes`
//!   (`u64`), then the channel count (`u32`).
//! - [`Credit`]: kind 3, then `limit_bytes` (`u64`).
//! - [`Reply`]: kind 1 (opened), then `live` (`u64`). Kind 2 (head): path (`u8`, live
//!   0, backfill 1), seq (`u64`), count (`u32`), and the series count (`u32`). Kind 3
//!   is retired.
//! - [`keys`]: each channel key (`u128`).
//! - [`ends`]: place and end (each `u32`) for each series.

pub mod client;
pub mod forward;
mod frames;
mod gaps;
mod home;
mod part;
mod reader;
pub mod write;

use std::fmt;
use std::num::NonZeroUsize;

pub use frames::{Due, Frames, Step};
pub use gaps::Gaps;
pub use home::{FromReader, Home};
pub use reader::{FromHome, Reader};
use types::frame::{Path, Range};

use crate::common::{Fields, Length, Run, Writer, body};

const LATEST: u8 = 1;
const COMPLETE: u8 = 2;
const CREDIT: u8 = 3;

const OPENED: u8 = 1;
const HEAD: u8 = 2;
// Reply kind 3 is retired: no reply takes it.

/// Stop code: the home does not know a channel of the open.
pub const UNKNOWN: u32 = 16;
/// Stop code: the node is not the home of the open's index.
pub const NOT_HOME: u32 = 17;
/// Stop code: the home's buffer failed, or its mesh stopped.
pub const FAILED: u32 = 18;
/// Stop code: the side that stops had no block for a stream's session, or no room for
/// a request body under its cap. A later open or request can succeed, but not when
/// the block is larger than each block of that side's pool.
pub const BUSY: u32 = 19;

/// A code of HUB WIRE that ends a session, from the side that stops the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// [`MALFORMED`](crate::header::MALFORMED).
    Malformed,
    /// [`UNKNOWN`].
    Unknown,
    /// [`NOT_HOME`].
    NotHome,
    /// [`FAILED`].
    Failed,
    /// [`BUSY`].
    Busy,
}

impl Refusal {
    /// The refusal that `code` names, or `None` for a code that HUB WIRE does not
    /// name.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            crate::header::MALFORMED => Some(Self::Malformed),
            UNKNOWN => Some(Self::Unknown),
            NOT_HOME => Some(Self::NotHome),
            FAILED => Some(Self::Failed),
            BUSY => Some(Self::Busy),
            _ => None,
        }
    }

    /// The code that stops or resets the stream.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Malformed => crate::header::MALFORMED,
            Self::Unknown => UNKNOWN,
            Self::NotHome => NOT_HOME,
            Self::Failed => FAILED,
            Self::Busy => BUSY,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "a message broke the hub protocol",
            Self::Unknown => "the home does not know a channel of the open",
            Self::NotHome => "the node is not the home of the index",
            Self::Failed => "the home's buffer failed, or its mesh stopped",
            Self::Busy => "the side that stopped had no block for the session",
        })
    }
}

/// The first message from the reader's node, which opens the session. The run of its
/// [`keys`] follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Open {
    /// Which frames the session gets.
    pub mode: Mode,
    /// The count of channels the reader reads, all on one index. At least 1.
    pub channels: u32,
}

impl Open {
    /// The bytes of the encoded open.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self.mode {
            Mode::Latest => 5,
            Mode::Complete { .. } => 13,
        }
    }

    /// Writes the open into `out`.
    ///
    /// # Panics
    ///
    /// When `channels` is 0, or `out` is not [`Open::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(self.channels > 0, "an open names at least one channel");
        let mut out = Writer::new(out, self.encoded_len());
        out.put(&[self.kind()]);
        if let Mode::Complete { limit_bytes } = self.mode {
            out.put(&limit_bytes.to_le_bytes());
        }
        out.put(&self.channels.to_le_bytes());
    }

    fn kind(&self) -> u8 {
        match self.mode {
            Mode::Latest => LATEST,
            Mode::Complete { .. } => COMPLETE,
        }
    }

    /// Decodes the open in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no open, [`Error::Length`] when the length fits no open of that kind, and
    /// [`Error::Channels`] when the open names no channel.
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(bytes);
        let Some(kind) = fields.kind() else {
            return Err(Error::Empty);
        };
        let mode = match kind {
            LATEST => Mode::Latest,
            COMPLETE => Mode::Complete {
                limit_bytes: u64::from_le_bytes(*fields.take()?),
            },
            kind => return Err(Error::Kind { kind }),
        };
        let channels = u32::from_le_bytes(*fields.take()?);
        fields.end()?;
        if channels == 0 {
            return Err(Error::Channels);
        }
        Ok(Self { mode, channels })
    }
}

/// Which frames a reader session gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The newest live frame, before its commit.
    Latest,
    /// Each live frame after its commit.
    Complete {
        /// The first grant of credit, in bytes.
        limit_bytes: u64,
    },
}

/// Each message from the reader's node after the open's keys: the session's total
/// grant since the open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Credit {
    /// The grant, in bytes.
    pub limit_bytes: u64,
}

impl Credit {
    /// The bytes of an encoded credit.
    pub const LEN: usize = 9;

    /// Writes the credit into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Credit::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[CREDIT]);
        out.put(&self.limit_bytes.to_le_bytes());
    }

    /// Decodes the credit in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no credit, and [`Error::Length`] when `bytes` is not [`Credit::LEN`]
    /// bytes.
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(bytes);
        let Some(kind) = fields.kind() else {
            return Err(Error::Empty);
        };
        if kind != CREDIT {
            return Err(Error::Kind { kind });
        }
        let limit_bytes = u64::from_le_bytes(*fields.take()?);
        fields.end()?;
        Ok(Self { limit_bytes })
    }
}

/// A message from the home to the reader's node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The session is open at the home.
    Opened {
        /// The first live seq of the index that the session can get: its open
        /// position. A latest session opens at the first seq of the newest frame of
        /// the index, or at the live tail when there is none.
        live: u64,
    },
    /// The head of one frame. The run of its [`ends`] follows, then its body.
    Head(Head),
}

/// The head of one frame that the home sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// How the frame reached the home.
    pub path: Path,
    /// The samples of the frame. A head whose range ends past `u64::MAX` is not
    /// valid.
    pub range: Range,
    /// The count of the frame's series, which is the count of its ends. At least 1,
    /// since a frame holds its index. A head with more series than the session has
    /// places is not valid.
    pub series: u32,
}

impl Reply {
    /// The bytes of the encoded reply.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Opened { .. } => 9,
            Self::Head(_) => 18,
        }
    }

    /// Writes the reply into `out`.
    ///
    /// # Panics
    ///
    /// When a head has no series or a range that ends past `u64::MAX`, or `out` is
    /// not [`Reply::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match self {
            Self::Opened { live } => {
                out.put(&[OPENED]);
                out.put(&live.to_le_bytes());
            }
            Self::Head(head) => {
                assert!(head.series > 0, "a head names at least one series");
                assert!(
                    head.range.end().is_some(),
                    "a head's range ends past the highest seq: {} samples from seq {}",
                    head.range.count,
                    head.range.seq
                );
                out.put(&[HEAD, path_byte(head.path)]);
                out.put(&head.range.seq.to_le_bytes());
                out.put(&head.range.count.to_le_bytes());
                out.put(&head.series.to_le_bytes());
            }
        }
    }

    /// Decodes the reply in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no reply, [`Error::Length`] when the length fits no reply of that kind,
    /// [`Error::Path`] when a head names no path, [`Error::Series`] when it names no
    /// series, and [`Error::Range`] when its range ends past `u64::MAX`.
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(bytes);
        let Some(kind) = fields.kind() else {
            return Err(Error::Empty);
        };
        match kind {
            OPENED => {
                let live = u64::from_le_bytes(*fields.take()?);
                fields.end()?;
                Ok(Self::Opened { live })
            }
            HEAD => {
                let [path] = *fields.take()?;
                let seq = u64::from_le_bytes(*fields.take()?);
                let count = u32::from_le_bytes(*fields.take()?);
                let series = u32::from_le_bytes(*fields.take()?);
                fields.end()?;
                let path = path_of(path)?;
                if series == 0 {
                    return Err(Error::Series);
                }
                let range = Range { seq, count };
                if range.end().is_none() {
                    return Err(Error::Range { seq, count });
                }
                Ok(Self::Head(Head {
                    path,
                    range,
                    series,
                }))
            }
            kind => Err(Error::Kind { kind }),
        }
    }
}

/// The run of keys after an open, in the open's order. A series of the session has
/// the place of its channel's first key in the run, from 0. The run holds the key of
/// the open's index. [`Home`] does not check this: it does not know the index, so its
/// caller does.
pub mod keys {
    use std::slice;

    use types::channel;

    use super::{Error, run, slots};

    /// The bytes of one key.
    pub const LEN: usize = 16;

    /// Writes `keys` into `out`, one message of the run.
    ///
    /// # Panics
    ///
    /// When `keys` is empty, or `out` is not 16 bytes for each key.
    pub fn encode(keys: &[channel::Key], out: &mut [u8]) {
        let slots = slots::<LEN>(out);
        assert!(
            slots.len() == keys.len(),
            "out holds {} keys, and the message has {}",
            slots.len(),
            keys.len()
        );
        for (out, key) in slots.iter_mut().zip(keys) {
            *out = key.as_u128().to_le_bytes();
        }
    }

    /// The keys of one message of the run, in order.
    #[derive(Clone, Debug)]
    pub struct Iter<'m>(slice::Iter<'m, [u8; LEN]>);

    impl Iterator for Iter<'_> {
        type Item = channel::Key;

        fn next(&mut self) -> Option<channel::Key> {
            let &key = self.0.next()?;
            Some(channel::Key::from_u128(u128::from_le_bytes(key)))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.0.size_hint()
        }
    }

    impl ExactSizeIterator for Iter<'_> {}

    /// The keys in `message`, one message of the run.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `message` is empty, and [`Error::Length`] when it is not
    /// a whole count of keys.
    pub(super) fn decode(message: &[u8]) -> Result<Iter<'_>, Error> {
        run::<LEN>(message).map(|keys| Iter(keys.iter()))
    }
}

/// The run of ends after a head: the place of each series and the end of its bytes in
/// the body, in increasing place order. The body follows, as long as the last end,
/// laid out as the series bytes of a frame of only these series in place order: the
/// first starts at 0, and each other at the end before it rounded up to a multiple of
/// 8. The padding may hold any bytes. An end whose place the session does not have,
/// that is not above the place before it, or that is below the start of its series, is
/// not valid.
pub mod ends {
    use std::slice;

    use super::{Error, run, slots};

    /// The bytes of one end.
    pub const LEN: usize = 8;

    /// Writes the first `out.len() / LEN` ends of `ends`, each a place and an end, into
    /// `out`, one message of the run. It takes no more, so a caller that passes
    /// `by_ref()` writes the rest into the next message.
    ///
    /// # Panics
    ///
    /// When `out` is empty or not a whole count of ends, or when `ends` gives fewer ends
    /// than `out` holds.
    pub fn encode(ends: impl IntoIterator<Item = (u32, u32)>, out: &mut [u8]) {
        let slots = slots::<LEN>(out);
        let count = slots.len();
        let written = slots
            .iter_mut()
            .zip(ends)
            .map(|(slot, (place, end))| {
                let [p0, p1, p2, p3] = place.to_le_bytes();
                let [e0, e1, e2, e3] = end.to_le_bytes();
                *slot = [p0, p1, p2, p3, e0, e1, e2, e3];
            })
            .count();
        assert!(
            written == count,
            "ends gave {written} of the {count} ends that out holds"
        );
    }

    /// The ends of one message of the run, in order, each a place and an end.
    #[derive(Clone, Debug)]
    pub struct Iter<'m>(slice::Iter<'m, [u8; LEN]>);

    impl Iterator for Iter<'_> {
        type Item = (u32, u32);

        fn next(&mut self) -> Option<(u32, u32)> {
            self.0.next().copied().map(end)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.0.size_hint()
        }
    }

    impl ExactSizeIterator for Iter<'_> {}

    impl Iter<'_> {
        /// The last end of the message.
        pub(super) fn last_end(&self) -> Option<u32> {
            self.0.as_slice().last().map(|&bytes| end(bytes).1)
        }
    }

    /// The ends in `message`, one message of the run.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `message` is empty, and [`Error::Length`] when it is not
    /// a whole count of ends.
    pub(super) fn decode(message: &[u8]) -> Result<Iter<'_>, Error> {
        run::<LEN>(message).map(|ends| Iter(ends.iter()))
    }

    fn end([p0, p1, p2, p3, e0, e1, e2, e3]: [u8; LEN]) -> (u32, u32) {
        (
            u32::from_le_bytes([p0, p1, p2, p3]),
            u32::from_le_bytes([e0, e1, e2, e3]),
        )
    }
}

/// A message of a reader session or of a write stream that is not valid, or a write
/// stream that ends inside a frame.
/// The decoders of [`client`] give [`client::Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        /// The bytes of the message, or of the counts after the last end of a message
        /// of the run of ends.
        len: usize,
    },
    /// An open names no channel.
    Channels,
    /// A forward names no place.
    Unplaced,
    /// A head names no series.
    Series,
    /// The range of a head or of a write's reply ends past the highest seq.
    Range {
        /// The seq of the range.
        seq: u64,
        /// The count of the range.
        count: u32,
    },
    /// A head names no path: no read head has this path byte, and no write head has
    /// this time byte.
    Path {
        /// The path byte.
        byte: u8,
    },
    /// A message opens a session that is open: a second open or opened.
    Reopen {
        /// The kind byte of the message.
        kind: u8,
    },
    /// A message comes before the session is open: a credit before the open, or a
    /// head before opened.
    Unopened {
        /// The kind byte of the message.
        kind: u8,
    },
    /// A head has more series than the session has places.
    Places {
        /// The series of the head.
        series: u32,
        /// The places of the session.
        places: u32,
    },
    /// A place of a forward is not after the place before it, or not below the
    /// channels of the open.
    Place {
        /// The place.
        place: u32,
    },
    /// A head of a reader session is on the backfill path. A session gets only live
    /// frames.
    Backfill,
    /// A head starts before the end of the head before it, or the first head before
    /// the open position. The heads of a session rise from that position.
    Seq {
        /// The seq of the head.
        seq: u64,
        /// The seq plus the count of the head before it, or the open position.
        end: u64,
    },
    /// A message of a run has more keys, ends, counts, or places than remain in the
    /// run.
    Run {
        /// The keys, ends, counts, or places of the message.
        items: usize,
        /// The keys, ends, counts, or places that remain in the run.
        remain: u32,
    },
    /// A message of a body has more bytes than remain in the body.
    Body {
        /// The bytes of the message.
        len: usize,
        /// The bytes that remain in the body.
        remain: usize,
    },
    /// A message that only a complete session has comes in a latest session.
    Latest {
        /// The kind byte of the message.
        kind: u8,
    },
    /// The stream ended before its body.
    Unfinished {
        /// The bytes of the body that did not come.
        remain: usize,
    },
    /// The home finished the stream outside a body.
    Finished,
    /// The range of a write's reply holds no sample.
    Count {
        /// The seq of the range.
        seq: u64,
    },
    /// A write's reply refuses a frame with a code that no
    /// [`write::Refusal`] has.
    Refusal {
        /// The code.
        code: u8,
    },
    /// A write's reply ends a group with a code that no [`write::End`] has.
    End {
        /// The code.
        code: u8,
    },
    /// A count of a write's run of counts is 0.
    Samples,
    /// A write's head names no form.
    Form {
        /// The form byte.
        byte: u8,
    },
    /// A write's head of [`write::Time::Node`] has the encoded form.
    Stamp,
    /// The program finished its write stream inside a frame.
    Cut {
        /// The bytes of the frame that came: its head, its ends, its counts, and its
        /// body.
        received: usize,
        /// The bytes of the frame, once its ends are whole.
        len: Option<NonZeroUsize>,
    },
    /// An outcome of a write stream answers a frame that the program did not send
    /// whole.
    Unasked {
        /// The frame that it answers.
        frame: u64,
        /// The frames that the program sent whole.
        sent: u64,
    },
    /// An outcome of a write stream answers a frame that the program sent, other than
    /// the one that the next outcome answers.
    Order {
        /// The frame that it answers.
        frame: u64,
        /// The frame that the next outcome answers.
        next: u64,
    },
    /// An outcome of a write stream answers the frame that the next outcome answers,
    /// and another group than the one that it answers.
    Group {
        /// The frame that it answers.
        frame: u64,
        /// The group that it answers.
        group: u32,
        /// The group that the next outcome answers.
        next: u32,
    },
    /// A `Stored` of a write stream covers fewer frames than the last `Stored`, or
    /// more than got an outcome.
    Stored {
        /// The frames that it covers.
        frames: u64,
        /// The frames that the last `Stored` covers when `frames` is below them, else
        /// the frames that got an outcome.
        bound: u64,
    },
}

// Frame decoders give it for each message: it stays `Copy`, with no drop glue, and 24
// bytes, so each frame decoder's result stays small.
const _: fn() = || {
    fn copy<T: Copy>() {}
    copy::<Error>();
};
const _: () = assert!(size_of::<Error>() == 24, "an error stays 24 bytes");

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
            Self::Channels => f.write_str("the hub open names no channel"),
            Self::Unplaced => f.write_str("the hub forward names no place"),
            Self::Series => f.write_str("the frame head names no series"),
            Self::Range { seq, count } => write!(
                f,
                "the range has {count} samples from seq {seq}, which end past the \
                 highest seq"
            ),
            Self::Path { byte } => write!(
                f,
                "the frame head names path or time {byte}, which this node does not \
                 know"
            ),
            Self::Reopen { kind } => write!(
                f,
                "the hub message has kind {kind}, which opens the session, and the \
                 session is open"
            ),
            Self::Unopened { kind } => write!(
                f,
                "the hub message has kind {kind}, and the session is not open"
            ),
            Self::Places { series, places } => write!(
                f,
                "the frame head names {series} series, and the session has {places} \
                 places"
            ),
            Self::Place { place } => write!(
                f,
                "the forward names place {place}, which is not after the place before \
                 it or not below the channels of the open"
            ),
            Self::Backfill => {
                f.write_str("the frame head of a reader session is on backfill")
            }
            Self::Seq { seq, end } => write!(
                f,
                "the frame head starts at seq {seq}, before seq {end}, where the head \
                 before it ended or the session opened"
            ),
            Self::Run { items, remain } => write!(
                f,
                "the run message holds {items} items, and {remain} remain in the run"
            ),
            Self::Body { len, remain } => write!(
                f,
                "the body message has {len} bytes, and {remain} remain in the body"
            ),
            Self::Latest { kind } => write!(
                f,
                "the hub message has kind {kind}, which a latest session does not have"
            ),
            Self::Unfinished { remain } => write!(
                f,
                "the stream ended with {remain} bytes of its body to come"
            ),
            Self::Finished => {
                f.write_str("the home finished the stream outside a body")
            }
            Self::Count { seq } => {
                write!(f, "the reply's range from seq {seq} holds no sample")
            }
            Self::Refusal { code } => {
                write!(f, "the refused frame has code {code}, which no refusal has")
            }
            Self::End { code } => {
                write!(f, "the ended group has code {code}, which no end has")
            }
            Self::Samples => f.write_str("the frame holds a group of no sample"),
            Self::Form { byte } => {
                write!(f, "the frame head names form {byte}, which no frame has")
            }
            Self::Stamp => f.write_str(
                "the frame head that the node stamps is in the encoded form, not the \
                 raw form",
            ),
            Self::Cut {
                received,
                len: Some(len),
            } => write!(
                f,
                "the stream ended inside a frame: {received} of {len} bytes"
            ),
            Self::Cut {
                received,
                len: None,
            } => write!(
                f,
                "the stream ended inside a frame: {received} bytes, before the end of \
                 its ends"
            ),
            Self::Unasked { frame, sent } => write!(
                f,
                "the outcome answers frame {frame}, not one of the {sent} frames sent"
            ),
            Self::Order { frame, next } => write!(
                f,
                "the outcome answers frame {frame}, not frame {next}, which the next \
                 outcome answers"
            ),
            Self::Group { frame, group, next } => write!(
                f,
                "the outcome answers group {group} of frame {frame}, not group {next}, \
                 which the next outcome answers"
            ),
            Self::Stored { frames, bound } if frames < bound => write!(
                f,
                "the stored reply covers {frames} frames, fewer than the {bound} of \
                 the last stored reply"
            ),
            Self::Stored { frames, bound } => write!(
                f,
                "the stored reply covers {frames} frames, more than the {bound} that \
                 got an outcome"
            ),
        }
    }
}

impl std::error::Error for Error {}

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

/// The slots of one message of a run: `out` as items of `N` bytes, to fill.
///
/// # Panics
///
/// When `out` is empty or not a whole count of items.
fn slots<const N: usize>(out: &mut [u8]) -> &mut [[u8; N]] {
    let len = out.len();
    let (slots, rest) = out.as_chunks_mut::<N>();
    assert!(
        rest.is_empty(),
        "out has {len} bytes, not a whole count of {N}-byte items"
    );
    assert!(
        !slots.is_empty(),
        "a message of a run holds at least one item"
    );
    slots
}

/// The items of `N` bytes in `message`, one message of a run.
fn run<const N: usize>(message: &[u8]) -> Result<&[[u8; N]], Error> {
    let (items, rest) = message.as_chunks::<N>();
    if message.is_empty() {
        Err(Error::Empty)
    } else if !rest.is_empty() {
        Err(Error::Length { len: message.len() })
    } else {
        Ok(items)
    }
}

fn path_byte(path: Path) -> u8 {
    match path {
        Path::Live => 0,
        Path::Backfill => 1,
    }
}

fn path_of(byte: u8) -> Result<Path, Error> {
    match byte {
        0 => Ok(Path::Live),
        1 => Ok(Path::Backfill),
        byte => Err(Error::Path { byte }),
    }
}

#[cfg(test)]
mod tests {
    use std::iter;

    use proptest::prelude::*;
    use types::channel;

    use super::*;

    pub(super) fn key(bits: u128) -> channel::Key {
        channel::Key::from_u128(bits)
    }

    fn head(path: Path, seq: u64, count: u32, series: u32) -> Reply {
        Reply::Head(Head {
            path,
            range: Range { seq, count },
            series,
        })
    }

    fn live() -> Reply {
        head(Path::Live, 0, 0, 1)
    }

    pub(super) fn encode_open(open: Open) -> Vec<u8> {
        let mut out = vec![0xaa; open.encoded_len()];
        open.encode(&mut out);
        out
    }

    pub(super) fn encode_credit(credit: Credit) -> Vec<u8> {
        let mut out = vec![0xaa; Credit::LEN];
        credit.encode(&mut out);
        out
    }

    pub(super) fn encode_reply(reply: Reply) -> Vec<u8> {
        let mut out = vec![0xaa; reply.encoded_len()];
        reply.encode(&mut out);
        out
    }

    pub(super) fn encode_keys(keys: &[channel::Key]) -> Vec<u8> {
        let mut out = vec![[0xaa; 16]; keys.len()].concat();
        super::keys::encode(keys, &mut out);
        out
    }

    pub(super) fn encode_ends(ends: &[(u32, u32)]) -> Vec<u8> {
        let mut out = vec![[0xaa; 8]; ends.len()].concat();
        super::ends::encode(ends.iter().copied(), &mut out);
        out
    }

    /// `items` cut into messages, each of the next size in `sizes`, the last of what
    /// remains.
    pub(super) fn cut<'i, T>(
        mut items: &'i [T],
        sizes: &mut impl Iterator<Item = usize>,
    ) -> Vec<&'i [T]> {
        let mut messages = Vec::new();
        while !items.is_empty() {
            let size = sizes.next().expect("the sizes repeat").min(items.len());
            let (message, rest) = items.split_at(size);
            messages.push(message);
            items = rest;
        }
        messages
    }

    /// `kind`, then `len - 1` zeros.
    fn zeros(kind: u8, len: usize) -> Vec<u8> {
        iter::once(kind).chain(iter::repeat(0)).take(len).collect()
    }

    /// `decode` gives [`Error::Length`] for a message of `kind` with each length in
    /// `lens`.
    pub(super) fn check<T: fmt::Debug + PartialEq>(
        decode: fn(&[u8]) -> Result<T, Error>,
        kind: u8,
        lens: &[usize],
    ) {
        for &len in lens {
            let bytes = zeros(kind, len);
            assert_eq!(decode(&bytes), Err(Error::Length { len }), "kind {kind}");
        }
    }

    /// `decode` gives [`Error::Kind`] for each kind byte outside `known`, at each
    /// length in `lens`.
    pub(super) fn check_kinds<T: fmt::Debug + PartialEq>(
        decode: fn(&[u8]) -> Result<T, Error>,
        known: &[u8],
        lens: &[usize],
    ) {
        for kind in (0..=u8::MAX).filter(|kind| !known.contains(kind)) {
            for &len in lens {
                let bytes = zeros(kind, len);
                assert_eq!(decode(&bytes), Err(Error::Kind { kind }), "{len}");
            }
        }
    }

    mod open {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let latest = Open {
                mode: Mode::Latest,
                channels: 0x0102_0304,
            };
            assert_eq!(encode_open(latest), [1, 4, 3, 2, 1]);
            let complete = Open {
                mode: Mode::Complete {
                    limit_bytes: 0x0102_0304_0506_0708,
                },
                channels: 2,
            };
            assert_eq!(
                encode_open(complete),
                [2, 8, 7, 6, 5, 4, 3, 2, 1, 2, 0, 0, 0]
            );
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(Open::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(Open::decode, &[1, 2], &[1, 5, 13]);
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(Open::decode, 1, &[1, 4, 6, 13]);
            check(Open::decode, 2, &[1, 5, 9, 12, 14]);
        }

        #[test]
        fn refuses_an_open_of_no_channel() {
            assert_eq!(Open::decode(&[1, 0, 0, 0, 0]), Err(Error::Channels));
            assert_eq!(Open::decode(&zeros(2, 13)), Err(Error::Channels));
        }

        #[test]
        fn decodes_an_open_of_one_channel() {
            let open = Open {
                mode: Mode::Latest,
                channels: 1,
            };
            assert_eq!(Open::decode(&[1, 1, 0, 0, 0]), Ok(open));
        }

        #[test]
        #[should_panic(expected = "an open names at least one channel")]
        fn panics_on_an_open_of_no_channel() {
            let open = Open {
                mode: Mode::Latest,
                channels: 0,
            };
            open.encode(&mut [0; 5]);
        }

        #[test]
        #[should_panic(expected = "out has 4 bytes, and the message has 5")]
        fn panics_when_out_has_the_wrong_length() {
            let open = Open {
                mode: Mode::Latest,
                channels: 1,
            };
            open.encode(&mut [0; 4]);
        }
    }

    mod credit {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let credit = Credit {
                limit_bytes: 0x0102_0304_0506_0708,
            };
            assert_eq!(encode_credit(credit), [3, 8, 7, 6, 5, 4, 3, 2, 1]);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(Credit::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(Credit::decode, &[3], &[1, 5, 9]);
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(Credit::decode, 3, &[1, 8, 10]);
        }

        #[test]
        #[should_panic(expected = "out has 10 bytes, and the message has 9")]
        fn panics_when_out_has_the_wrong_length() {
            Credit { limit_bytes: 1 }.encode(&mut [0; 10]);
        }
    }

    mod reply {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            assert_eq!(
                encode_reply(Reply::Opened {
                    live: 0x0102_0304_0506_0708
                }),
                [1, 8, 7, 6, 5, 4, 3, 2, 1]
            );
            let backfill = head(Path::Backfill, 0x0102_0304_0506_0708, 0x0a0b_0c0d, 3);
            assert_eq!(
                encode_reply(backfill),
                [
                    2, 1, 8, 7, 6, 5, 4, 3, 2, 1, 0x0d, 0x0c, 0x0b, 0x0a, 3, 0, 0, 0
                ]
            );
            let mut live_bytes = zeros(2, 18);
            live_bytes[14] = 1;
            assert_eq!(encode_reply(live()), live_bytes);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(Reply::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(Reply::decode, &[1, 2], &[1, 9, 18]);
        }

        #[test]
        fn refuses_the_retired_kind_of_behind() {
            assert_eq!(Reply::decode(&[3]), Err(Error::Kind { kind: 3 }));
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(Reply::decode, 1, &[1, 2, 8, 10, 18]);
            check(Reply::decode, 2, &[1, 2, 17, 19]);
        }

        #[test]
        fn refuses_an_opened_of_one_byte() {
            let error = Reply::decode(&[1]).expect_err("an opened has 9 bytes");
            assert_eq!(error, Error::Length { len: 1 });
            assert_eq!(
                error.to_string(),
                "the hub message has 1 bytes, which no message of its kind has"
            );
        }

        #[test]
        fn refuses_a_head_of_no_series() {
            assert_eq!(Reply::decode(&zeros(2, 18)), Err(Error::Series));
        }

        #[test]
        fn refuses_a_head_whose_range_ends_past_the_highest_seq() {
            let mut bytes = encode_reply(head(Path::Live, u64::MAX - 1, 1, 1));
            assert_eq!(
                Reply::decode(&bytes),
                Ok(head(Path::Live, u64::MAX - 1, 1, 1))
            );
            bytes[10] = 2;
            assert_eq!(
                Reply::decode(&bytes),
                Err(Error::Range {
                    seq: u64::MAX - 1,
                    count: 2
                })
            );
        }

        #[test]
        fn decodes_a_head_of_one_series() {
            let mut bytes = zeros(2, 18);
            bytes[14] = 1;
            assert_eq!(Reply::decode(&bytes), Ok(live()));
        }

        #[test]
        #[should_panic(expected = "a head names at least one series")]
        fn panics_on_a_head_of_no_series() {
            head(Path::Live, 0, 0, 0).encode(&mut [0; 18]);
        }

        #[test]
        #[should_panic(
            expected = "a head's range ends past the highest seq: 1 samples from seq \
                        18446744073709551615"
        )]
        fn panics_on_a_head_whose_range_ends_past_the_highest_seq() {
            head(Path::Live, u64::MAX, 1, 1).encode(&mut [0; 18]);
        }

        #[test]
        fn refuses_unknown_paths() {
            for byte in 2..=u8::MAX {
                let mut bytes = encode_reply(live());
                bytes[1] = byte;
                assert_eq!(Reply::decode(&bytes), Err(Error::Path { byte }));
            }
        }

        #[test]
        fn checks_the_length_before_the_path() {
            assert_eq!(Reply::decode(&[2, 9, 9]), Err(Error::Length { len: 3 }));
        }

        #[test]
        fn checks_the_path_before_the_series() {
            let mut bytes = zeros(2, 18);
            bytes[1] = 2;
            assert_eq!(Reply::decode(&bytes), Err(Error::Path { byte: 2 }));
        }

        #[test]
        #[should_panic(expected = "out has 19 bytes, and the message has 18")]
        fn panics_when_out_has_the_wrong_length() {
            live().encode(&mut [0; 19]);
        }
    }

    mod keys {
        use super::*;

        fn decode(message: &[u8]) -> Result<Vec<channel::Key>, Error> {
            super::super::keys::decode(message).map(Iterator::collect)
        }

        #[test]
        fn pins_the_wire_values() {
            let keys = [key(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10), key(1)];
            let mut bytes = vec![16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
            bytes.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(encode_keys(&keys), bytes);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn decodes_each_message_of_a_split_run() {
            let sent: Vec<_> = (0..200).map(key).collect();
            let run = encode_keys(&sent);
            let got: Vec<_> = run
                .chunks(1472 / super::super::keys::LEN * super::super::keys::LEN)
                .flat_map(|message| super::super::keys::decode(message).unwrap())
                .collect();
            assert_eq!(got, sent);
        }

        #[test]
        fn refuses_a_partial_key() {
            for len in [1, 15, 17, 31, 33] {
                assert_eq!(decode(&vec![0; len]), Err(Error::Length { len }));
            }
        }

        #[test]
        #[should_panic(expected = "a message of a run holds at least one item")]
        fn panics_on_no_key() {
            super::super::keys::encode(&[], &mut []);
        }

        #[test]
        #[should_panic(
            expected = "out has 15 bytes, not a whole count of 16-byte items"
        )]
        fn panics_on_a_partial_key_in_out() {
            super::super::keys::encode(&[key(1)], &mut [0; 15]);
        }

        #[test]
        #[should_panic(expected = "out holds 2 keys, and the message has 1")]
        fn panics_when_out_holds_another_count_of_keys() {
            super::super::keys::encode(&[key(1)], &mut [0; 32]);
        }

        #[test]
        #[should_panic(expected = "out holds 1 keys, and the message has 2")]
        fn panics_when_out_holds_fewer_keys_than_the_message() {
            super::super::keys::encode(&[key(1), key(2)], &mut [0; 16]);
        }
    }

    mod ends {
        use super::*;

        fn decode(message: &[u8]) -> Result<Vec<(u32, u32)>, Error> {
            super::super::ends::decode(message).map(Iterator::collect)
        }

        #[test]
        fn pins_the_wire_values() {
            assert_eq!(
                encode_ends(&[(3, 8), (0, 21)]),
                [3, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 21, 0, 0, 0]
            );
        }

        #[test]
        fn gives_the_body_length_as_the_last_end() {
            let message = encode_ends(&[(1, 8), (0, u32::MAX)]);
            let last = super::super::ends::decode(&message).map(|ends| ends.last_end());
            assert_eq!(last, Ok(Some(u32::MAX)));
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn decodes_each_message_of_a_split_run() {
            let sent: Vec<_> = (0..400).map(|place| (place, (place + 1) * 8)).collect();
            let run = encode_ends(&sent);
            let got: Vec<_> = run
                .chunks(1472 / super::super::ends::LEN * super::super::ends::LEN)
                .flat_map(|message| super::super::ends::decode(message).unwrap())
                .collect();
            assert_eq!(got, sent);
        }

        #[test]
        fn starts_each_series_on_8_bytes() {
            let check = |ends: &[(u32, u32)], len: usize| {
                let message = encode_ends(ends);
                let ends = super::super::ends::decode(&message)
                    .expect("the run has ends")
                    .map(|(place, end)| (place, usize::try_from(end).expect("fits")));
                types::frame::check(&vec![0xff; len], ends)
            };
            assert_eq!(check(&[(0, 3), (1, 13)], 13), Ok(()));
            assert_eq!(
                check(&[(0, 3), (1, 5)], 5),
                Err(types::frame::BadEnd::Before { end: 5, start: 8 })
            );
        }

        #[test]
        fn refuses_a_partial_end() {
            for len in [1, 7, 9, 15, 17] {
                assert_eq!(decode(&vec![0; len]), Err(Error::Length { len }));
            }
        }

        #[test]
        #[should_panic(expected = "a message of a run holds at least one item")]
        fn panics_on_no_end() {
            super::super::ends::encode(iter::empty(), &mut []);
        }

        #[test]
        #[should_panic(expected = "out has 9 bytes, not a whole count of 8-byte items")]
        fn panics_on_a_partial_end_in_out() {
            super::super::ends::encode([(0, 1)], &mut [0; 9]);
        }

        #[test]
        #[should_panic(expected = "out has 7 bytes, not a whole count of 8-byte items")]
        fn panics_on_an_out_shorter_than_an_end() {
            super::super::ends::encode([(0, 1)], &mut [0; 7]);
        }

        #[test]
        #[should_panic(expected = "ends gave 1 of the 2 ends that out holds")]
        fn panics_on_fewer_ends_than_out_holds() {
            super::super::ends::encode([(0, 1)], &mut [0; 16]);
        }

        #[test]
        fn keeps_the_ends_that_out_does_not_hold() {
            let mut ends = [(0, 1), (1, 2), (2, 3)].into_iter();
            let mut out = [0xaa; 8];
            super::super::ends::encode(ends.by_ref(), &mut out);
            assert_eq!(out.as_slice(), encode_ends(&[(0, 1)]));
            assert_eq!(ends.collect::<Vec<_>>(), [(1, 2), (2, 3)]);
        }

        #[test]
        fn takes_no_end_past_out_from_an_iterator_of_no_known_length() {
            let mut ends = [(0, 1), (1, 2), (2, 3)].into_iter().filter(|_| true);
            super::super::ends::encode(ends.by_ref(), &mut [0xaa; 8]);
            assert_eq!(ends.collect::<Vec<_>>(), [(1, 2), (2, 3)]);
        }

        #[test]
        #[should_panic(expected = "ends gave 1 of the 3 ends that out holds")]
        fn panics_on_one_end_for_an_out_of_three() {
            super::super::ends::encode([(0, 1)], &mut [0; 24]);
        }

        #[test]
        #[should_panic(expected = "a message of a run holds at least one item")]
        fn panics_on_an_empty_out_with_an_end() {
            super::super::ends::encode([(0, 1)], &mut []);
        }

        #[test]
        #[should_panic(
            expected = "out has 17 bytes, not a whole count of 8-byte items"
        )]
        fn panics_on_a_partial_end_after_two_ends() {
            super::super::ends::encode([(0, 1), (1, 2)], &mut [0; 17]);
        }

        #[test]
        fn writes_a_run_split_into_two_messages_from_one_iterator() {
            let sent: Vec<_> = (0..400).map(|place| (place, (place + 1) * 8)).collect();
            let mut ends = sent.iter().copied();
            let mut run = vec![0xaa; sent.len() * super::super::ends::LEN];
            let (first, second) = run.split_at_mut(184 * super::super::ends::LEN);
            super::super::ends::encode(ends.by_ref(), first);
            super::super::ends::encode(ends.by_ref(), second);
            assert_eq!(ends.next(), None);
            assert_eq!(run, encode_ends(&sent));
        }

        #[test]
        fn writes_ends_in_place_order_from_a_list_by_place() {
            // Home entry 0 is place 2, entry 1 is place 0, entry 2 is place 1.
            let lens = [3, 16, 1];
            let mut places = [(2, 0), (0, 1), (1, 2)];
            places.sort_unstable();
            let ends = types::frame::ends(
                places.iter().map(|&(place, entry)| (place, lens[entry])),
            )
            .map(|(place, end)| (place, u32::try_from(end).expect("fits")));
            let mut out = [0xaa; 24];
            super::super::ends::encode(ends, &mut out);
            assert_eq!(out.as_slice(), encode_ends(&[(0, 16), (1, 17), (2, 27)]));
        }
    }

    #[test]
    fn pins_the_stop_codes() {
        assert_eq!((UNKNOWN, NOT_HOME), (16, 17));
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one case for each error")]
    fn names_each_error() {
        let cases = [
            (Error::Empty, "the hub message is empty"),
            (
                Error::Kind { kind: 9 },
                "the hub message has kind 9, which this stream does not carry",
            ),
            (
                Error::Length { len: 4 },
                "the hub message has 4 bytes, which no message of its kind has",
            ),
            (Error::Channels, "the hub open names no channel"),
            (Error::Unplaced, "the hub forward names no place"),
            (Error::Series, "the frame head names no series"),
            (
                Error::Range { seq: 7, count: 2 },
                "the range has 2 samples from seq 7, which end past the highest seq",
            ),
            (
                Error::Path { byte: 2 },
                "the frame head names path or time 2, which this node does not know",
            ),
            (
                Error::Reopen { kind: 1 },
                "the hub message has kind 1, which opens the session, and the session \
                 is open",
            ),
            (
                Error::Unopened { kind: 3 },
                "the hub message has kind 3, and the session is not open",
            ),
            (
                Error::Places {
                    series: 4,
                    places: 3,
                },
                "the frame head names 4 series, and the session has 3 places",
            ),
            (
                Error::Place { place: 3 },
                "the forward names place 3, which is not after the place before it or \
                 not below the channels of the open",
            ),
            (
                Error::Backfill,
                "the frame head of a reader session is on backfill",
            ),
            (
                Error::Seq { seq: 3, end: 11 },
                "the frame head starts at seq 3, before seq 11, where the head before it ended \
                 or the session opened",
            ),
            (
                Error::Run {
                    items: 3,
                    remain: 2,
                },
                "the run message holds 3 items, and 2 remain in the run",
            ),
            (
                Error::Body {
                    len: 11,
                    remain: 10,
                },
                "the body message has 11 bytes, and 10 remain in the body",
            ),
            (
                Error::Latest { kind: 3 },
                "the hub message has kind 3, which a latest session does not have",
            ),
            (
                Error::Unfinished { remain: 3 },
                "the stream ended with 3 bytes of its body to come",
            ),
            (
                Error::Finished,
                "the home finished the stream outside a body",
            ),
            (
                Error::Count { seq: 7 },
                "the reply's range from seq 7 holds no sample",
            ),
            (
                Error::Refusal { code: 8 },
                "the refused frame has code 8, which no refusal has",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    fn open() -> impl Strategy<Value = Open> {
        let mode = prop_oneof![
            Just(Mode::Latest),
            any::<u64>().prop_map(|limit_bytes| Mode::Complete { limit_bytes }),
        ];
        (mode, 1..=u32::MAX).prop_map(|(mode, channels)| Open { mode, channels })
    }

    fn reply() -> impl Strategy<Value = Reply> {
        let path = prop_oneof![Just(Path::Live), Just(Path::Backfill)];
        prop_oneof![
            any::<u64>().prop_map(|live| Reply::Opened { live }),
            (path, any::<u64>(), any::<u32>(), 1..=u32::MAX).prop_map(
                |(path, seq, count, series)| {
                    head(
                        path,
                        seq.min(u64::MAX.saturating_sub(u64::from(count))),
                        count,
                        series,
                    )
                }
            ),
        ]
    }

    /// A kind byte from 0 to 4, then random bytes, most of them a length that a
    /// message of some kind has. The byte after the kind is most often a path.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let rest = prop_oneof![Just(4), Just(8), Just(12), Just(17), 0..24_usize];
        let path = prop_oneof![3 => 0..2_u8, 1 => any::<u8>()];
        (0..5_u8, rest, path).prop_flat_map(|(kind, rest, path)| {
            proptest::collection::vec(any::<u8>(), rest).prop_map(move |mut rest| {
                if let Some(first) = rest.first_mut() {
                    *first = path;
                }
                [[kind].as_slice(), &rest].concat()
            })
        })
    }

    #[test]
    fn names_each_code_of_hub_wire_that_ends_a_session() {
        let refusals = [
            (Refusal::Malformed, 2, "a message broke the hub protocol"),
            (
                Refusal::Unknown,
                16,
                "the home does not know a channel of the open",
            ),
            (
                Refusal::NotHome,
                17,
                "the node is not the home of the index",
            ),
            (
                Refusal::Failed,
                18,
                "the home's buffer failed, or its mesh stopped",
            ),
            (
                Refusal::Busy,
                19,
                "the side that stopped had no block for the session",
            ),
        ];
        for (refusal, code, meaning) in refusals {
            assert_eq!(refusal.code(), code);
            assert_eq!(Refusal::from_code(code), Some(refusal));
            assert_eq!(refusal.to_string(), meaning);
        }
        for code in (0..32).filter(|code| ![2, 16, 17, 18, 19].contains(code)) {
            assert_eq!(Refusal::from_code(code), None, "{code}");
        }
    }

    proptest! {
        #[test]
        fn round_trips_an_open(open in open()) {
            prop_assert_eq!(Open::decode(&encode_open(open)), Ok(open));
        }

        #[test]
        fn round_trips_a_credit(limit_bytes in any::<u64>()) {
            let credit = Credit { limit_bytes };
            prop_assert_eq!(Credit::decode(&encode_credit(credit)), Ok(credit));
        }

        #[test]
        fn round_trips_a_reply(reply in reply()) {
            prop_assert_eq!(Reply::decode(&encode_reply(reply)), Ok(reply));
        }

        #[test]
        fn encodes_no_reply_of_the_retired_kind(reply in reply()) {
            prop_assert_ne!(encode_reply(reply)[0], 3);
        }

        #[test]
        fn round_trips_keys(bits in proptest::collection::vec(any::<u128>(), 1..8)) {
            let keys: Vec<_> = bits.into_iter().map(key).collect();
            let decoded = super::keys::decode(&encode_keys(&keys))
                .map(Iterator::collect::<Vec<_>>);
            prop_assert_eq!(decoded, Ok(keys));
        }

        #[test]
        fn round_trips_ends(
            ends in proptest::collection::vec(any::<(u32, u32)>(), 1..8),
        ) {
            let decoded = super::ends::decode(&encode_ends(&ends))
                .map(Iterator::collect::<Vec<_>>);
            prop_assert_eq!(decoded, Ok(ends));
        }

        #[test]
        fn decodes_only_the_messages_it_encodes(bytes in bytes()) {
            if let Ok(open) = Open::decode(&bytes) {
                prop_assert_eq!(&encode_open(open), &bytes);
            }
            if let Ok(credit) = Credit::decode(&bytes) {
                prop_assert_eq!(&encode_credit(credit), &bytes);
            }
            if let Ok(reply) = Reply::decode(&bytes) {
                prop_assert_eq!(&encode_reply(reply), &bytes);
            }
        }
    }
}
