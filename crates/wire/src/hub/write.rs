//! The frames of a program's write stream after its places, and the replies of the
//! node. The messages before them are in [`client::write`](super::client::write).
//!
//! Fields are little-endian.
//!
//! - [`Head`]: kind 7, the time (`u8`, live 0, backfill 1, node 2), the form (`u8`, raw
//!   0, encoded 1), and the count of series (`u32`).
//! - The run of counts: after the run of ends, the samples of each series of each
//!   present group (`u32`), in group order. A head of [`Time::Node`] has none. After
//!   the head, each message holds whole ends, then whole counts.
//! - [`Lead`]: the head, the run of ends, and the run of counts of one frame.
//! - [`Reply`]: taken 1 or lost 2, then the frame (`u64`), the group (`u32`), the seq
//!   (`u64`), and the count (`u32`); refused 3, then the frame, the group, and the code
//!   (`u8`, waiting 1, reserved 2, expired 3, order 4, codec 5, full 6, large 7); ended
//!   5, then the frame, the group, and the code (`u8`, removed 1, failed 2); stored 4,
//!   then the count of frames that it covers (`u64`).

use std::collections::VecDeque;
use std::iter;
use std::ops::Range;

use types::frame::key_set::KeySet;
use types::frame::{self, Form, Path};

use super::{Error, Fields, Writer, ends, slots};

mod frames;

pub use frames::{Due, Frames, Step};

const HEAD: u8 = 7;

const LIVE: u8 = 0;
const BACKFILL: u8 = 1;
const NODE: u8 = 2;

const RAW: u8 = 0;
const ENCODED: u8 = 1;

const TAKEN: u8 = 1;
const LOST: u8 = 2;
const REFUSED: u8 = 3;
const STORED: u8 = 4;
const ENDED: u8 = 5;

/// The head of one frame that a program writes, after the places. The run of its
/// [`ends`](super::ends) follows, then its run of counts, then its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// Where the frame's stamps come from, and its path.
    pub time: Time,
    /// How the frame's series hold their samples.
    pub form: Form,
    /// The count of the frame's series, at least 1 and at most the count of places.
    pub series: u32,
}

/// Where the stamps of a frame come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Time {
    /// The frame's index series holds them. The frame goes to this path.
    Frame(Path),
    /// The node writes a stamp over the one sample of each index of the frame, which
    /// holds any 8 bytes. The frame goes to the live path, holds one sample of each
    /// series in the raw form, and has no run of counts.
    Node,
}

impl Head {
    /// The bytes of an encoded head.
    pub const LEN: usize = 7;

    /// Writes the head into `out`.
    ///
    /// # Panics
    ///
    /// When `series` is 0, when a head of [`Time::Node`] has the encoded form, or when
    /// `out` is not [`Head::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(self.series > 0, "a head has no series");
        assert!(
            self.time != Time::Node || self.form == Form::Raw,
            "a head that the node stamps is in the encoded form"
        );
        let time = match self.time {
            Time::Frame(Path::Live) => LIVE,
            Time::Frame(Path::Backfill) => BACKFILL,
            Time::Node => NODE,
        };
        let form = match self.form {
            Form::Raw => RAW,
            Form::Encoded => ENCODED,
        };
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[HEAD, time, form]);
        out.put(&self.series.to_le_bytes());
    }

    /// Decodes a head, and refuses each head that [`Head::encode`] panics on.
    fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(message);
        match fields.kind().ok_or(Error::Empty)? {
            HEAD => {}
            kind => return Err(Error::Kind { kind }),
        }
        let [time, form] = *fields.take()?;
        let series = u32::from_le_bytes(*fields.take()?);
        fields.end()?;
        let time = match time {
            LIVE => Time::Frame(Path::Live),
            BACKFILL => Time::Frame(Path::Backfill),
            NODE => Time::Node,
            byte => return Err(Error::Path { byte }),
        };
        let form = match form {
            RAW => Form::Raw,
            ENCODED => Form::Encoded,
            byte => return Err(Error::Form { byte }),
        };
        if series == 0 {
            return Err(Error::Series);
        }
        if time == Time::Node && form != Form::Raw {
            return Err(Error::Stamp);
        }
        Ok(Self { time, form, series })
    }
}

/// The lead of one frame that a program writes: its head, its run of ends, and, for
/// [`Time::Frame`], its run of counts.
#[derive(Clone, Copy, Debug)]
pub struct Lead<'f> {
    time: Time,
    frame: &'f frame::Frame,
}

impl<'f> Lead<'f> {
    /// The most bytes of the lead of a frame whose places are the entries of `set`.
    #[must_use]
    pub fn most(set: &KeySet) -> usize {
        let ends = ends::LEN.strict_mul(set.entries().len());
        let counts = counts::LEN.strict_mul(set.groups().len());
        Head::LEN.strict_add(ends).strict_add(counts)
    }

    /// The lead of `frame`, whose stamps come from `time`.
    ///
    /// # Panics
    ///
    /// When the path of `time` is not the path of `frame` (the live path for
    /// [`Time::Node`]), or when `time` is [`Time::Node`] and `frame` is in the
    /// encoded form or holds other than one sample of each series.
    #[must_use]
    pub fn new(time: Time, frame: &'f frame::Frame) -> Self {
        let path = match time {
            Time::Frame(path) => path,
            Time::Node => Path::Live,
        };
        assert_eq!(
            frame.path(),
            path,
            "the frame is not on the path of its time"
        );
        if time == Time::Node {
            assert_eq!(
                frame.form(),
                Form::Raw,
                "a frame that the node stamps is in the encoded form"
            );
            assert!(
                frame.ranges().all(|(_, range)| range.count == 1),
                "a frame that the node stamps holds other than one sample of each \
                 series"
            );
        }
        Self { time, frame }
    }

    /// The bytes of the encoded lead.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let counts = match self.time {
            Time::Frame(_) => self.frame.ranges().count(),
            Time::Node => 0,
        };
        self.ended().strict_add(counts.strict_mul(counts::LEN))
    }

    /// Writes the lead into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Lead::bytes`] bytes, or when a group of the frame holds no
    /// sample.
    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(out.len(), self.bytes(), "out is not the bytes of the lead");
        let to_u32 = |n: usize| u32::try_from(n).expect("a frame's ends fit a u32");
        let (head, rest) = out.split_at_mut(Head::LEN);
        Head {
            time: self.time,
            form: self.frame.form(),
            series: to_u32(self.frame.ends().count()),
        }
        .encode(head);
        let (run, rest) = rest.split_at_mut(self.ended().strict_sub(Head::LEN));
        let ended = self
            .frame
            .ends()
            .map(|(entry, end)| (to_u32(entry), to_u32(end)));
        ends::encode(ended, run);
        if !rest.is_empty() {
            counts::encode(self.frame.ranges().map(|(_, range)| range.count), rest);
        }
    }

    /// The messages of the encoded lead for a stream whose messages hold at most
    /// `bytes_max` bytes, as ranges of it, in order. The head is one message, and
    /// each other message holds whole ends, then whole counts.
    ///
    /// # Panics
    ///
    /// When `bytes_max` is under [`Head::LEN`] or [`ends::LEN`](super::ends::LEN).
    pub fn messages(&self, bytes_max: usize) -> impl Iterator<Item = Range<usize>> {
        assert!(
            bytes_max >= Head::LEN.max(ends::LEN),
            "a message of {bytes_max} bytes holds no head or no end"
        );
        let (ended, bytes) = (self.ended(), self.bytes());
        let whole = |room: usize, len: usize| room.strict_div(len).strict_mul(len);
        let mut at = Head::LEN;
        let rest = iter::from_fn(move || {
            let start = at;
            let ends = ended.saturating_sub(at).min(whole(bytes_max, ends::LEN));
            at = at.strict_add(ends);
            if at >= ended {
                let room = whole(bytes_max.strict_sub(ends), counts::LEN);
                at = at.strict_add(bytes.strict_sub(at).min(room));
            }
            (at > start).then_some(start..at)
        });
        iter::once(0..Head::LEN).chain(rest)
    }

    /// The bytes of the head and the run of ends.
    fn ended(&self) -> usize {
        Head::LEN.strict_add(self.frame.ends().count().strict_mul(ends::LEN))
    }
}

/// The run of counts after the run of ends of a head of [`Time::Frame`]: the samples
/// of each series of each present group, at least 1, in group order. A group is
/// present when the ends hold its index. The run starts in the message of the last
/// end when that message has room, and each message holds whole counts.
mod counts {
    use super::slots;

    /// The bytes of one count.
    pub(super) const LEN: usize = 4;

    /// Writes the first `out.len() / LEN` counts of `counts` into `out`, one message of
    /// the run. It takes no more, so a caller that passes `by_ref()` writes the rest
    /// into the next message.
    ///
    /// # Panics
    ///
    /// When `out` is empty or not a whole count of counts, when `counts` gives fewer
    /// counts than `out` holds, or when a count is 0.
    pub(super) fn encode(counts: impl IntoIterator<Item = u32>, out: &mut [u8]) {
        let slots = slots::<LEN>(out);
        let len = slots.len();
        let written = slots
            .iter_mut()
            .zip(counts)
            .map(|(slot, count)| {
                assert!(count > 0, "a group of a frame holds no sample");
                *slot = count.to_le_bytes();
            })
            .count();
        assert!(
            written == len,
            "counts gave {written} of the {len} counts that out holds"
        );
    }
}

/// A reply of the node on a write stream after the places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// What the home of one group did with one frame. The outcomes of a frame come in
    /// group order, one for each present group.
    Outcome {
        /// The frame that it answers, from 1: the count of frames of the stream
        /// through it.
        frame: u64,
        /// The group of the frame that it answers: a position in
        /// [`KeySet::groups`](types::frame::key_set::KeySet::groups).
        group: u32,
        /// What the home did with the group.
        outcome: Outcome,
    },
    /// Each frame among the first `frames` of the stream whose group 0 its home took is
    /// on disk. `frames` counts each frame whose groups each got an [`Outcome`], in
    /// their order, and never goes back. A node takes a stream of one group, so it is
    /// the `Stored` of group 0 alone.
    Stored {
        /// The count of frames that it covers.
        frames: u64,
    },
}

/// What the home of one group did with one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The home applied the group at `range`. It is not on disk yet.
    Taken {
        /// The seqs of the frame's samples: at least 1, and they end at most at
        /// `u64::MAX`.
        range: frame::Range,
    },
    /// A live group found no room: `range` is a gap in the log.
    Lost {
        /// The seqs of the frame's samples: at least 1, and they end at most at
        /// `u64::MAX`.
        range: frame::Range,
    },
    /// The home refused the group, and no seq moved.
    Refused(Refusal),
    /// The write of the group ended before this frame, and no seq moved. Each later
    /// frame that holds the group gets the same outcome. The other groups go on.
    Ended(End),
}

impl Outcome {
    /// The most bytes that a [`Reply::Outcome`] encodes to.
    pub const LEN_MAX: usize = 25;
}

/// Why the write of a group ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// A channel of the group was removed.
    Removed,
    /// The home of the group failed.
    Failed,
}

impl End {
    const ALL: [Self; 2] = [Self::Removed, Self::Failed];

    const fn code(self) -> u8 {
        match self {
            Self::Removed => 1,
            Self::Failed => 2,
        }
    }
}

/// Why the home of a group refused a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Another writer holds control.
    Waiting,
    /// Control is held for a writer from before a restart.
    Reserved,
    /// The writer's control lease ran out. Open the writer again.
    Expired,
    /// A stamp is not after the stamp before it.
    Order,
    /// A series is not a series of its channel's type.
    Codec,
    /// A backfill frame found no room in the log, or the frame found no block of the
    /// node's pool. Write it again later.
    Full,
    /// The frame is too large for one write. Split it.
    Large,
}

impl Refusal {
    const ALL: [Self; 7] = [
        Self::Waiting,
        Self::Reserved,
        Self::Expired,
        Self::Order,
        Self::Codec,
        Self::Full,
        Self::Large,
    ];

    const fn code(self) -> u8 {
        match self {
            Self::Waiting => 1,
            Self::Reserved => 2,
            Self::Expired => 3,
            Self::Order => 4,
            Self::Codec => 5,
            Self::Full => 6,
            Self::Large => 7,
        }
    }
}

impl Reply {
    /// The bytes of the encoded reply.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        match self {
            Self::Outcome {
                outcome: Outcome::Taken { .. } | Outcome::Lost { .. },
                ..
            } => Outcome::LEN_MAX,
            Self::Outcome {
                outcome: Outcome::Refused(_) | Outcome::Ended(_),
                ..
            } => 14,
            Self::Stored { .. } => 9,
        }
    }

    /// Writes the reply into `out`.
    ///
    /// # Panics
    ///
    /// When a range holds no sample or ends past `u64::MAX`, or `out` is not
    /// [`Reply::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        let (frame, group, outcome) = match *self {
            Self::Outcome {
                frame,
                group,
                outcome,
            } => (frame, group, outcome),
            Self::Stored { frames } => {
                out.put(&[STORED]);
                out.put(&frames.to_le_bytes());
                return;
            }
        };
        let kind = match outcome {
            Outcome::Taken { .. } => TAKEN,
            Outcome::Lost { .. } => LOST,
            Outcome::Refused(_) => REFUSED,
            Outcome::Ended(_) => ENDED,
        };
        out.put(&[kind]);
        out.put(&frame.to_le_bytes());
        out.put(&group.to_le_bytes());
        match outcome {
            Outcome::Taken { range } | Outcome::Lost { range } => {
                put_range(&mut out, range);
            }
            Outcome::Refused(refusal) => out.put(&[refusal.code()]),
            Outcome::Ended(end) => out.put(&[end.code()]),
        }
    }

    /// Decodes a reply.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a kind that no reply has,
    /// [`Error::Length`], [`Error::Count`] for a range that holds no sample,
    /// [`Error::Range`] for a range that ends past `u64::MAX`, [`Error::Refusal`] for
    /// a refusal code that no refusal has, and [`Error::End`] for an end code that no
    /// end has.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(message);
        let kind = fields.kind().ok_or(Error::Empty)?;
        if kind == STORED {
            let frames = u64::from_le_bytes(*fields.take()?);
            fields.end()?;
            return Ok(Self::Stored { frames });
        }
        if !matches!(kind, TAKEN | LOST | REFUSED | ENDED) {
            return Err(Error::Kind { kind });
        }
        let frame = u64::from_le_bytes(*fields.take()?);
        let group = u32::from_le_bytes(*fields.take()?);
        let outcome = match kind {
            TAKEN => Outcome::Taken {
                range: take_range(&mut fields)?,
            },
            LOST => Outcome::Lost {
                range: take_range(&mut fields)?,
            },
            _ => {
                let [code] = *fields.take()?;
                fields.end()?;
                if kind == REFUSED {
                    Outcome::Refused(
                        Refusal::ALL
                            .into_iter()
                            .find(|refusal| refusal.code() == code)
                            .ok_or(Error::Refusal { code })?,
                    )
                } else {
                    Outcome::Ended(
                        End::ALL
                            .into_iter()
                            .find(|end| end.code() == code)
                            .ok_or(Error::End { code })?,
                    )
                }
            }
        };
        Ok(Self::Outcome {
            frame,
            group,
            outcome,
        })
    }
}

/// The order of the replies of a write stream at the program: an outcome answers the
/// oldest group of a frame sent whole with no outcome, and names it, and
/// [`Reply::Stored`] never passes the frames answered nor goes back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Replies {
    sent: u64,
    /// The frame and group of each outcome due, oldest first.
    due: VecDeque<(u64, u32)>,
    /// The count of frames whose groups each got an outcome.
    answered: u64,
    /// The count of frames up to the last that a home took a group of.
    taken: u64,
    stored: u64,
}

impl Replies {
    /// The replies of a stream that sent no frame.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts a frame that the program sent whole, whose present groups are `groups`,
    /// in group order.
    ///
    /// # Panics
    ///
    /// When `groups` is empty.
    pub fn sent(&mut self, groups: impl IntoIterator<Item = u32>) {
        let frame = self.sent.strict_add(1);
        let due = self.due.len();
        self.due
            .extend(groups.into_iter().map(|group| (frame, group)));
        assert!(self.due.len() > due, "a frame sent holds no group");
        self.sent = frame;
    }

    /// Decodes `message`, the next reply, and checks its order.
    ///
    /// # Errors
    ///
    /// As [`Reply::decode`], [`Error::Unasked`] for an outcome of frame 0 or of a frame
    /// past those sent, [`Error::Order`] for an outcome of another frame than the
    /// oldest sent with an outcome due, [`Error::Group`] for an outcome of another
    /// group of that frame than the oldest due, and [`Error::Stored`] for a
    /// [`Reply::Stored`] past the frames answered or below the last
    /// [`Reply::Stored`]. A reply that it refuses changes no count.
    #[inline(always)]
    pub fn decode(&mut self, message: &[u8]) -> Result<Reply, Error> {
        let reply = Reply::decode(message)?;
        match reply {
            Reply::Outcome { frame, .. } if frame == 0 || frame > self.sent => {
                return Err(Error::Unasked {
                    frame,
                    sent: self.sent,
                });
            }
            Reply::Outcome {
                frame,
                group,
                outcome,
            } => {
                let (next, due) = self
                    .due
                    .front()
                    .copied()
                    .unwrap_or((self.sent.strict_add(1), 0));
                if frame != next {
                    return Err(Error::Order { frame, next });
                }
                if group != due {
                    return Err(Error::Group {
                        frame,
                        group,
                        next: due,
                    });
                }
                self.due.pop_front();
                if self.due.front().is_none_or(|&(next, _)| next != frame) {
                    self.answered = frame;
                }
                if let Outcome::Taken { .. } = outcome {
                    self.taken = frame;
                }
            }
            Reply::Stored { frames } if frames < self.stored => {
                let bound = self.stored;
                return Err(Error::Stored { frames, bound });
            }
            Reply::Stored { frames } if frames > self.answered => {
                let bound = self.answered;
                return Err(Error::Stored { frames, bound });
            }
            Reply::Stored { frames } => self.stored = frames,
        }
        Ok(reply)
    }

    /// Whether each group of each frame sent got its outcome.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.answered == self.sent
    }

    /// Whether each group of each frame sent got its outcome, and the last
    /// [`Reply::Stored`] covers each frame that a home took a group of: each frame
    /// whose group 0 its home took is on disk.
    #[must_use]
    pub fn stored(&self) -> bool {
        self.answered() && self.stored >= self.taken
    }
}

fn put_range(out: &mut Writer<'_>, range: frame::Range) {
    assert!(
        range.count > 0 && range.end().is_some(),
        "a reply's range of {} samples from seq {} holds none or ends past the highest \
         seq",
        range.count,
        range.seq
    );
    out.put(&range.seq.to_le_bytes());
    out.put(&range.count.to_le_bytes());
}

/// The range after the kind, the last field of its reply.
fn take_range(fields: &mut Fields<'_>) -> Result<frame::Range, Error> {
    let seq = u64::from_le_bytes(*fields.take()?);
    let count = u32::from_le_bytes(*fields.take()?);
    fields.end()?;
    let range = frame::Range { seq, count };
    if count == 0 {
        return Err(Error::Count { seq });
    }
    if range.end().is_none() {
        return Err(Error::Range { seq, count });
    }
    Ok(range)
}

#[cfg(test)]
mod tests;
