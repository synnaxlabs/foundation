//! The write of one group of a write stream.

use std::cell::RefCell;
use std::ops::Range;

use block::Unique;
use types::frame::key_set::KeySet;
use types::frame::{Draft, Label, Layout, Path};
use types::time::{Span, Stamp};
use wire::hub::write::{End, Outcome, Refusal, Reply, Time};

use super::to_usize;
use crate::State;
use crate::writer::{Failure, Writer};

/// The bytes of the block of a `Stored` reply, which also holds an `Ended` reply.
const STORED_BYTES: usize = Reply::Stored {
    group: 0,
    frames: 0,
}
.encoded_len();

/// A frame of the stream, for the write of one group.
pub(super) enum Frame<'a> {
    /// A frame of the writer's set: the stream has one group.
    Whole(Draft),
    /// A frame of the stream's set, whose series of the group the write copies.
    Part {
        /// The frame.
        frame: &'a Draft,
        /// The entry in the writer's set and the length of each series of the group.
        series: &'a [(usize, usize)],
        /// The position in `frame` of each of `series`, as [`Draft::nth`] takes it.
        sources: &'a [usize],
    },
}

/// The write of one group of a stream at its home, and the `Stored` of the frames that
/// it took, until the group ends.
pub(super) struct Group {
    /// The group in the stream's set.
    number: u32,
    /// The writer, until the group ends or the stream finishes.
    writer: Option<Writer>,
    /// Why the write of the group ended.
    ended: Option<End>,
    /// The last stamp that the home took or lost, of a frame of [`Time::Node`].
    stamped: Option<Stamp>,
    /// The count of frames of the stream through the last one taken since the last
    /// [`cover`](Group::cover).
    taken: Option<u64>,
    /// The block of the next `Stored`. Outside [`Group::write`], it is `Some` exactly
    /// when `taken` is.
    next: Option<Unique>,
    /// The count of frames that the commit that is due covers, and the block of its
    /// `Stored`.
    covered: Option<(u64, Unique)>,
}

impl Group {
    /// The write of group `number` of the stream with `writer`, whose set is that
    /// group.
    pub(super) fn new(number: u32, writer: Writer) -> Self {
        Self {
            number,
            writer: Some(writer),
            ended: None,
            stamped: None,
            taken: None,
            next: None,
            covered: None,
        }
    }

    /// Whether the group took a frame since the last [`cover`](Self::cover).
    pub(super) fn waiting(&self) -> bool {
        self.taken.is_some()
    }

    /// Writes the series of the group in `frame`, frame `count` of the stream, of
    /// `time`, and gives its outcome, after the `Ended` reply and its block when the
    /// group ends at it. First takes the block of the next `Stored` from the home's
    /// pool when the group holds none, so each taken frame has the block of its
    /// `Stored`. A frame that finds no block for it or for the copy of its series is
    /// refused as `Full` or `Large`, and the home does not see it.
    pub(super) fn write(
        &mut self,
        state: &RefCell<State>,
        frame: Frame<'_>,
        time: Time,
        count: u64,
    ) -> (Option<(Unique, Reply)>, Outcome) {
        if let Some(end) = self.ended {
            return (None, Outcome::Ended(end));
        }
        if self.next.is_none() {
            self.next = state.borrow().alloc(STORED_BYTES).ok();
        }
        if self.next.is_none() {
            return (None, self.untaken(Outcome::Refused(Refusal::Full)));
        }
        let draft = match frame {
            Frame::Whole(draft) => draft,
            Frame::Part {
                frame,
                series,
                sources,
            } => match self.copy(state, frame, series, sources) {
                Ok(draft) => draft,
                Err(error) => return (None, self.untaken(refused(&error))),
            },
        };
        let writer = self
            .writer
            .as_mut()
            .expect("invariant: a group that did not end writes until the finish");
        match write(writer, draft, time, &mut self.stamped) {
            Ok(outcome) => (None, self.counted(count, outcome)),
            Err(end) => {
                let block = self
                    .next
                    .take()
                    .expect("invariant: a group writes with the block of its `Stored`");
                (Some(self.end(end, block)), Outcome::Ended(end))
            }
        }
    }

    /// The outcome of a frame whose draft found no block: `error`, or the end of the
    /// group.
    pub(super) fn refuse(&mut self, error: &block::Error) -> Outcome {
        match self.ended {
            Some(end) => Outcome::Ended(end),
            None => self.untaken(refused(error)),
        }
    }

    /// Moves the taken frames to the commit that is called next. Gives whether the
    /// group took a frame since the last call.
    pub(super) fn cover(&mut self) -> bool {
        let Some(frames) = self.taken.take() else {
            return false;
        };
        let block = self
            .next
            .take()
            .expect("invariant: a taken frame has the block of its `Stored`");
        self.covered = Some((frames, block));
        true
    }

    /// The reply of the end of the commit that covers the group, `committed`, with its
    /// block: `Stored`, or `Ended` of `Failed`. `None` when the group ended after the
    /// commit's call.
    pub(super) fn committed(
        &mut self,
        committed: &Result<(), env::files::Error>,
    ) -> Option<(Unique, Reply)> {
        let (frames, block) = self.covered.take()?;
        if committed.is_err() {
            return Some(self.end(End::Failed, block));
        }
        let group = self.number;
        Some((block, Reply::Stored { group, frames }))
    }

    /// Drops the writer, so that control ends, and keeps the taken frames.
    pub(super) fn close(&mut self) {
        self.writer = None;
    }

    /// A draft of the writer's set from the home's pool, with `series` of the group,
    /// at `sources` in `from`, a frame of the stream's set.
    fn copy(
        &self,
        state: &RefCell<State>,
        from: &Draft,
        series: &[(usize, usize)],
        sources: &[usize],
    ) -> Result<Draft, block::Error> {
        let writer = self
            .writer
            .as_ref()
            .expect("invariant: a group that did not end writes until the finish");
        let mut draft = layout(writer.set(), series)
            .draft(state.borrow().home.pool(), from.form())?;
        for ((_, to), &source) in draft.iter_mut().zip(sources) {
            let (_, bytes) = from
                .nth(source)
                .expect("invariant: the frame holds each series of its split");
            to.copy_from_slice(bytes);
        }
        let range = from
            .range(self.number)
            .expect("invariant: the frame holds the group");
        draft.set_count(0, range.count);
        Ok(draft)
    }

    /// Ends the write of the group with `end`: drops the writer and each `Stored` to
    /// come, and gives `block` with the `Ended` reply.
    fn end(&mut self, end: End, block: Unique) -> (Unique, Reply) {
        self.ended = Some(end);
        self.writer = None;
        self.taken = None;
        self.next = None;
        self.covered = None;
        let group = self.number;
        (block, Reply::Ended { group, end })
    }

    /// Counts a taken frame for the next commit, and gives `outcome`.
    fn counted(&mut self, count: u64, outcome: Outcome) -> Outcome {
        if let Outcome::Taken { .. } = outcome {
            self.taken = Some(count);
            return outcome;
        }
        self.untaken(outcome)
    }

    /// Gives back the block of the next `Stored` while no taken frame waits for one,
    /// and gives `outcome`, of a frame that the home did not take.
    fn untaken(&mut self, outcome: Outcome) -> Outcome {
        if self.taken.is_none() {
            self.next = None;
        }
        outcome
    }
}

/// The series of each group of a frame of a stream of more than one group, in group
/// order, in buffers reused across frames.
pub(super) struct Parts {
    /// The group of each entry of the stream's set, and its entry in the set of the
    /// writer of that group, which holds the entries of the group in the same order.
    places: Box<[(u32, usize)]>,
    /// The span in `series` of each group of the last frame, by group.
    spans: Vec<Range<usize>>,
    /// The entry in the writer's set and the length of each series of the last frame.
    series: Vec<(usize, usize)>,
    /// The position in the frame of each of `series`.
    sources: Vec<usize>,
}

impl Parts {
    /// The parts of the frames of `set`, the stream's set.
    pub(super) fn new(set: &KeySet) -> Self {
        let mut counts = vec![0; set.groups().len()];
        let places = set
            .entries()
            .iter()
            .map(|entry| {
                let count = &mut counts[to_usize(entry.group)];
                let place = (entry.group, *count);
                *count += 1;
                place
            })
            .collect();
        Self {
            places,
            spans: vec![0..0; set.groups().len()],
            series: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// Splits `frame`, a frame of the stream's set whose groups are `present`, in
    /// rising order. Time is linear in the series of `frame`.
    pub(super) fn split(
        &mut self,
        frame: &Draft,
        present: impl Iterator<Item = u32> + Clone,
    ) {
        for group in present.clone() {
            self.spans[to_usize(group)] = 0..0;
        }
        for (source, _) in frame.iter() {
            self.spans[to_usize(self.places[source].0)].end += 1;
        }
        let mut start = 0;
        for group in present {
            let span = &mut self.spans[to_usize(group)];
            let count = span.end;
            *span = start..start;
            start += count;
        }
        self.series.resize(start, (0, 0));
        self.sources.resize(start, 0);
        for (n, (source, bytes)) in frame.iter().enumerate() {
            let (group, entry) = self.places[source];
            let span = &mut self.spans[to_usize(group)];
            self.series[span.end] = (entry, bytes.len());
            self.sources[span.end] = n;
            span.end += 1;
        }
    }

    /// The part of `group` of `frame`, the frame of the last [`split`](Self::split).
    pub(super) fn part<'a>(&'a self, frame: &'a Draft, group: u32) -> Frame<'a> {
        let span = self.spans[to_usize(group)].clone();
        Frame::Part {
            frame,
            series: &self.series[span.clone()],
            sources: &self.sources[span],
        }
    }
}

/// The layout of `series`, the series of one group of a frame, in `set`, that group.
fn layout<'a>(set: &'a KeySet, series: &'a [(usize, usize)]) -> Layout<'a> {
    Layout::new(set, series)
        .expect("invariant: the series of one group of a frame keep each rule")
}

/// The refusal of a frame whose draft found no block with `error`.
fn refused(error: &block::Error) -> Outcome {
    Outcome::Refused(match error {
        block::Error::TooLarge { .. } => Refusal::Large,
        block::Error::Exhausted { .. } | block::Error::Refused { .. } => Refusal::Full,
    })
}

/// Writes `draft`, a frame of `time`, with `writer`, and gives its outcome, or why the
/// write of its group ended. A frame of [`Time::Node`] gets its stamp from [`next`],
/// which `stamped` keeps when the home takes or loses the frame.
fn write(
    writer: &mut Writer,
    mut draft: Draft,
    time: Time,
    stamped: &mut Option<Stamp>,
) -> Result<Outcome, End> {
    let (label, stamp) = match time {
        Time::Frame(path) => (Label::Path(path), None),
        Time::Node => {
            let stamp = next(writer.now(), *stamped);
            let index = draft
                .series_mut(writer.set().groups()[0])
                .expect("invariant: a frame of a write holds its index");
            let stamp = stamp_index(index, stamp);
            (Label::Path(Path::Live), stamp)
        }
    };
    let outcome = outcome(&writer.write(label, draft))?;
    // The home spends the stamp of a lost frame too.
    if let Outcome::Taken { .. } | Outcome::Lost { .. } = outcome {
        *stamped = stamp.or(*stamped);
    }
    Ok(outcome)
}

/// The stamp of a frame of [`Time::Node`] at mesh time `now`: `now`, or 1 ns after
/// `stamped`, the last stamp of the group that the home took or lost, when `now` is
/// not after it.
fn next(now: Stamp, stamped: Option<Stamp>) -> Stamp {
    match stamped {
        Some(last) if now <= last => last + Span::from_nanos(1),
        _ => now,
    }
}

/// Writes `stamp` over the one sample of `index`, the index series of a frame, and
/// gives it. Gives `None` and writes nothing when that series is not one stamp, a frame
/// of one sample that the home refuses as `Codec` after its check of control.
fn stamp_index(index: &mut [u8], stamp: Stamp) -> Option<Stamp> {
    let sample = <&mut [u8; 8]>::try_from(index).ok()?;
    *sample = stamp.nanos().to_le_bytes();
    Some(stamp)
}

/// The outcome of a frame of one group from what the write gave, or why the write of
/// the group ends.
fn outcome(written: &Result<&[::home::Outcome], Failure>) -> Result<Outcome, End> {
    let outcomes = match *written {
        Ok(outcomes) => outcomes,
        Err(Failure::Removed(_)) => return Err(End::Removed),
        Err(Failure::Home(::home::Error::Disk(_))) => return Err(End::Failed),
        Err(Failure::Home(::home::Error::Full)) => {
            return Ok(Outcome::Refused(Refusal::Full));
        }
        Err(Failure::Home(::home::Error::Large)) => {
            return Ok(Outcome::Refused(Refusal::Large));
        }
        Err(Failure::Home(::home::Error::Resend)) => {
            unreachable!("invariant: each frame of a write is labeled with a path")
        }
    };
    let [outcome] = outcomes else {
        unreachable!("invariant: a frame of a group holds one group")
    };
    Ok(match *outcome {
        ::home::Outcome::Applied { range, .. } => Outcome::Taken { range },
        ::home::Outcome::Lost { range, .. } => Outcome::Lost { range },
        ::home::Outcome::Refused { ref refusal, .. } => {
            Outcome::Refused(match refusal {
                ::home::Refusal::Waiting => Refusal::Waiting,
                ::home::Refusal::Reserved => Refusal::Reserved,
                ::home::Refusal::Expired => Refusal::Expired,
                ::home::Refusal::Order(_) => Refusal::Order,
                ::home::Refusal::Codec { .. } => Refusal::Codec,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use types::channel::Slot;
    use types::frame::Range;

    use super::*;

    /// Each outcome and error of a write at the home gives the outcome of the same
    /// meaning, or the end of the group.
    #[test]
    fn gives_the_outcome_of_each_outcome_and_error_of_the_home() {
        let (slot, range) = (Slot::new(3), Range { seq: 4, count: 2 });
        let stamp = Stamp::from_nanos(9);
        let refused = |refusal| ::home::Outcome::Refused { slot, refusal };
        let codec = ::home::Refusal::Codec {
            channel: types::channel::Key::from_u128(2),
            error: codec::Error::Tag { vector: 0, tag: 9 },
        };
        let order = ::home::order::Error::Backwards {
            path: Path::Live,
            before: stamp,
            stamp,
        };
        let cases = [
            (
                ::home::Outcome::Applied { slot, range },
                Outcome::Taken { range },
            ),
            (
                ::home::Outcome::Lost { slot, range },
                Outcome::Lost { range },
            ),
            (
                refused(::home::Refusal::Waiting),
                Outcome::Refused(Refusal::Waiting),
            ),
            (
                refused(::home::Refusal::Reserved),
                Outcome::Refused(Refusal::Reserved),
            ),
            (
                refused(::home::Refusal::Expired),
                Outcome::Refused(Refusal::Expired),
            ),
            (
                refused(::home::Refusal::Order(order)),
                Outcome::Refused(Refusal::Order),
            ),
            (refused(codec), Outcome::Refused(Refusal::Codec)),
        ];
        for (home, want) in cases {
            assert_eq!(
                outcome(&Ok(std::slice::from_ref(&home))),
                Ok(want),
                "{home:?}"
            );
        }
        for (error, refusal) in [
            (::home::Error::Full, Refusal::Full),
            (::home::Error::Large, Refusal::Large),
        ] {
            let got = outcome(&Err(Failure::Home(error.clone())));
            assert_eq!(got, Ok(Outcome::Refused(refusal)), "{error:?}");
        }
        let removed = Failure::Removed(types::channel::Key::from_u128(2));
        assert_eq!(outcome(&Err(removed)), Err(End::Removed));
    }
}
