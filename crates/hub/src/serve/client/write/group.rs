//! The write of one group of a write stream.

use std::cell::RefCell;
use std::pin::Pin;
use std::task::{Context, Poll};

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
    /// A frame of the stream's set, whose series of the group, from [`split`], the
    /// write copies.
    Part(&'a mut Draft),
}

/// The write of one group of a stream at its home, and the `Stored` of each commit of
/// the frames that it took, until the group ends.
pub(super) struct Group {
    /// The group in the stream's set.
    number: u32,
    /// The writer, until the group ends or the stream finishes.
    writer: Option<Writer>,
    /// Why the write of the group ended.
    ended: Option<End>,
    /// The entry in the writer's set and the length of each series of the group in the
    /// last frame that [`split`] split, reused across frames.
    series: Vec<(usize, usize)>,
    /// The entry in the stream's set of each of `series`.
    sources: Vec<usize>,
    /// The last stamp that the home took or lost, of a frame of [`Time::Node`].
    stamped: Option<Stamp>,
    /// The count of frames of the stream through the last one taken since the call of
    /// the commit in `due`.
    taken: Option<u64>,
    /// The block of the next `Stored`. Outside [`Group::write`], it is `Some` exactly
    /// when `taken` is.
    next: Option<Unique>,
    /// The commit that is due, the count of frames that it covers, and the block of its
    /// `Stored`.
    due: Option<(::home::Commit, u64, Unique)>,
}

impl Group {
    /// The write of group `number` of the stream with `writer`, whose set is that
    /// group.
    pub(super) fn new(number: u32, writer: Writer) -> Self {
        Self {
            number,
            writer: Some(writer),
            ended: None,
            series: Vec::new(),
            sources: Vec::new(),
            stamped: None,
            taken: None,
            next: None,
            due: None,
        }
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
            Frame::Part(draft) => match self.copy(state, draft) {
                Ok(draft) => draft,
                Err(error) => return (None, self.untaken(refused(&error))),
            },
        };
        let writer = self
            .writer
            .as_mut()
            .expect("invariant: a group that did not end writes until the finish");
        match write(writer, draft, time, &mut self.stamped) {
            Ok(outcome) => (None, self.counted(state, count, outcome)),
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

    /// Polls the commit that is due. Pending when none is.
    pub(super) fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), env::files::Error>> {
        match &mut self.due {
            Some((commit, ..)) => Pin::new(commit).poll(cx),
            None => Poll::Pending,
        }
    }

    /// Whether a commit is due.
    pub(super) fn due(&self) -> bool {
        self.due.is_some()
    }

    /// Takes the due commit, which ended with `committed`, and gives the block and the
    /// reply of its end: `Stored` and the call of the next commit, or `Ended` of
    /// `Failed`.
    pub(super) fn committed(
        &mut self,
        state: &RefCell<State>,
        committed: &Result<(), env::files::Error>,
    ) -> (Unique, Reply) {
        let (_, frames, block) = self.due.take().expect("invariant: a commit is due");
        if committed.is_err() {
            return self.end(End::Failed, block);
        }
        self.call(state);
        let group = self.number;
        (block, Reply::Stored { group, frames })
    }

    /// Drops the writer, so that control ends, and keeps the commits that are due.
    pub(super) fn close(&mut self) {
        self.writer = None;
    }

    /// A draft of the writer's set from the home's pool, with the series of the group
    /// in `from`, a frame of the stream's set that [`split`] split.
    fn copy(
        &mut self,
        state: &RefCell<State>,
        from: &mut Draft,
    ) -> Result<Draft, block::Error> {
        let writer = self
            .writer
            .as_ref()
            .expect("invariant: a group that did not end writes until the finish");
        let mut draft = layout(writer.set(), &self.series)
            .draft(state.borrow().home.pool(), from.form())?;
        for ((_, to), &source) in draft.iter_mut().zip(&self.sources) {
            let bytes = from
                .series_mut(source)
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
        self.due = None;
        let group = self.number;
        (block, Reply::Ended { group, end })
    }

    /// Counts a taken frame for the next commit, which this calls when none is due, and
    /// gives `outcome`.
    fn counted(
        &mut self,
        state: &RefCell<State>,
        count: u64,
        outcome: Outcome,
    ) -> Outcome {
        if let Outcome::Taken { .. } = outcome {
            self.taken = Some(count);
            self.call(state);
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

    /// Calls a commit for the taken frames when none is due.
    fn call(&mut self, state: &RefCell<State>) {
        if self.due.is_none()
            && let Some(frames) = self.taken.take()
        {
            let block = self
                .next
                .take()
                .expect("invariant: a taken frame has the block of its `Stored`");
            self.due = Some((state.borrow().home.committed(), frames, block));
        }
    }
}

/// The group of each entry of `set`, the stream's set, and its entry in the set of the
/// writer of that group, which holds the entries of the group in the same order.
pub(super) fn places(set: &KeySet) -> Box<[(u32, usize)]> {
    let mut counts = vec![0; set.groups().len()];
    set.entries()
        .iter()
        .map(|entry| {
            let count = &mut counts[to_usize(entry.group)];
            let place = (entry.group, *count);
            *count += 1;
            place
        })
        .collect()
}

/// Gives each of `present`, the groups of `frame`, a frame of the stream's set, its
/// series in it, by `places` from [`places`].
pub(super) fn split(
    groups: &mut [Group],
    places: &[(u32, usize)],
    frame: &Draft,
    present: impl IntoIterator<Item = u32>,
) {
    for group in present {
        let group = &mut groups[to_usize(group)];
        group.series.clear();
        group.sources.clear();
    }
    for (source, bytes) in frame.iter() {
        let (group, entry) = places[source];
        let group = &mut groups[to_usize(group)];
        group.series.push((entry, bytes.len()));
        group.sources.push(source);
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
