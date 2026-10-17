//! Where each path of each index stands: with every appended entry, on disk, and
//! in which records.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::collections::VecDeque;
use std::collections::vec_deque::Drain;
use std::fmt;
use std::num::{NonZeroU8, NonZeroU64};
use std::ops::Deref;

use types::channel::{self, Slot};
use types::frame::Path;
use types::hash;
use types::time::Stamp;

use crate::carry;
use crate::entry::{Header, Kind, Samples};

/// Where one path of an index stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tail {
    /// The seq of the next entry.
    pub seq: u64,
    /// The `last` of the newest entry that has one, or `None` before it.
    pub stamp: Option<Stamp>,
}

/// An entry that cannot go on its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// The entry starts below the tail of its path.
    Below {
        index: channel::Key,
        path: Path,
        first: u64,
        tail: u64,
    },
    /// The entry ends past the last seq.
    Past {
        index: channel::Key,
        path: Path,
        first: u64,
        len: u32,
    },
    /// A carried tail is not the end of the path that its records give, or has
    /// another stamp than the path's stamp.
    Carried {
        carried: carry::Tail,
        end: Mark,
        stamp: Option<Stamp>,
    },
    /// A carry record lists the path twice.
    Repeated { index: channel::Key, path: Path },
    /// An entry with no samples follows `u64::MAX` of them at its seq on its path.
    Crowded {
        index: channel::Key,
        path: Path,
        seq: u64,
    },
    /// An entry with no samples counts other than the walk counts on its path.
    Counted {
        index: channel::Key,
        path: Path,
        seq: u64,
        given: u64,
        walked: u64,
    },
}

/// An [`Invalid`] of one path, without the index and the path, which
/// [`Logs::change`] adds.
#[derive(Clone, Copy, Debug)]
enum Fault {
    Below { first: u64, tail: u64 },
    Past { first: u64, len: u32 },
    Crowded { seq: u64 },
    Counted { seq: u64, given: u64, walked: u64 },
}

impl Fault {
    /// The error of the fault on `path` of `index`.
    const fn on(self, index: channel::Key, path: Path) -> Invalid {
        match self {
            Self::Below { first, tail } => Invalid::Below {
                index,
                path,
                first,
                tail,
            },
            Self::Past { first, len } => Invalid::Past {
                index,
                path,
                first,
                len,
            },
            Self::Crowded { seq } => Invalid::Crowded { index, path, seq },
            Self::Counted { seq, given, walked } => Invalid::Counted {
                index,
                path,
                seq,
                given,
                walked,
            },
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Below {
                index,
                path,
                first,
                tail,
            } => write!(
                f,
                "an entry of index {index} on path {path:?} starts at {first}, below \
                 the tail {tail}"
            ),
            Self::Past {
                index,
                path,
                first,
                len,
            } => write!(
                f,
                "an entry of index {index} on path {path:?} starts at {first} with \
                 {len} samples, past the last seq"
            ),
            Self::Carried {
                carried,
                end,
                stamp,
            } => write!(
                f,
                "the carried tail of index {} on path {:?} at seq {} after {} empty \
                 entries with stamp {:?} is not the end at seq {} after {} with stamp \
                 {stamp:?}",
                carried.index,
                carried.path,
                carried.seq,
                carried.given,
                carried.stamp,
                end.seq,
                end.given
            ),
            Self::Repeated { index, path } => write!(
                f,
                "a carry record lists index {index} on path {path:?} twice"
            ),
            Self::Crowded { index, path, seq } => write!(
                f,
                "an entry of index {index} on path {path:?} with no samples follows \
                 u64::MAX of them at seq {seq}"
            ),
            Self::Counted {
                index,
                path,
                seq,
                given,
                walked,
            } => write!(
                f,
                "an entry of index {index} on path {path:?} with no samples at seq \
                 {seq} counts {given}, and the walk counts {walked}"
            ),
        }
    }
}

/// Where a read of a path stands: before the entries that end past `seq`, and
/// past the first `given` entries with no samples at `seq`. Marks order as reads
/// do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mark {
    /// The seq that the entries the read has not given end past.
    pub seq: u64,
    /// How many entries with no samples at `seq` the read gave.
    pub given: u64,
}

impl Mark {
    /// The mark before every entry that ends past `seq`.
    #[must_use]
    pub const fn at(seq: u64) -> Self {
        Self { seq, given: 0 }
    }

    /// The mark after an entry of `len` samples from `first` that comes at this
    /// mark on its path.
    ///
    /// # Panics
    ///
    /// When `first + len` passes `u64::MAX`: the tail checked the entry. When
    /// `given` passes `u64::MAX`: the log checked the entry.
    pub(crate) fn after(self, first: u64, len: u32) -> Self {
        if len != 0 {
            let seq = first
                .checked_add(u64::from(len))
                .expect("invariant: the tail checked the entry");
            return Self::at(seq);
        }
        Self {
            seq: first,
            given: self
                .count(first)
                .expect("invariant: the log checked the entry")
                .get(),
        }
    }

    /// The count of an entry with no samples at `first` that comes at this mark on
    /// its path, or `None` past `u64::MAX`.
    fn count(self, first: u64) -> Option<NonZeroU64> {
        let given = if first == self.seq { self.given } else { 0 };
        NonZeroU64::MIN.checked_add(given)
    }
}

/// A record that holds entries of one path: the mark before the path's first
/// entry in it, and the record's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) start: Mark,
    pub(crate) offset: u64,
}

/// What a read of a path from a mark finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// The oldest record left with a durable entry that ends past the mark, with
    /// the index of the path.
    Run(channel::Key, Run),
    /// No record left has such an entry: the mark after the path's newest durable
    /// entry. No record holds the seqs from the read's mark up to it.
    End(Mark),
}

/// Where a path stands: its tail, and how many entries with no samples are at the
/// tail's seq.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Position {
    tail: Tail,
    given: u64,
}

impl Position {
    /// The mark after the newest entry.
    fn mark(self) -> Mark {
        Mark {
            seq: self.tail.seq,
            given: self.given,
        }
    }

    /// Moves past the entry of `samples` at `first`, and returns the mark before it
    /// and the kind of the entry's header: its samples, or its count. The tail's
    /// `seq` goes to `first` plus the `len` of `samples`, and its `stamp` to their
    /// `last` when they have one. A `first` past the tail is a skip ahead.
    ///
    /// # Errors
    ///
    /// [`Fault::Below`] when `first` is below the tail, [`Fault::Past`] when `first`
    /// plus `samples.len` does not fit in a `u64`, and [`Fault::Crowded`]. Nothing
    /// changes.
    fn advance(
        &mut self,
        first: u64,
        samples: Option<Samples>,
    ) -> Result<(Mark, Kind), Fault> {
        let start = self.mark();
        if first < start.seq {
            return Err(Fault::Below {
                first,
                tail: start.seq,
            });
        }
        let Some(samples) = samples else {
            let given = start.count(first).ok_or(Fault::Crowded { seq: first })?;
            self.tail.seq = first;
            self.given = given.get();
            return Ok((start, Kind::Record(given)));
        };
        let len = samples.len.get();
        self.tail.seq = first
            .checked_add(u64::from(len))
            .ok_or(Fault::Past { first, len })?;
        if let Some(last) = samples.last {
            self.tail.stamp = Some(last);
        }
        self.given = 0;
        Ok((start, Kind::Samples(samples)))
    }
}

/// One path of an index: where it stands with every appended entry, where it
/// stands on disk, and its runs, oldest first.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Log {
    index: channel::Key,
    path: Path,
    appended: Position,
    durable: Position,
    runs: VecDeque<Run>,
    /// The offset of the newest record of the path whose entry gives its durable
    /// stamp: a data record whose entry of the path gives that stamp, or a carry
    /// record of the path. A walk from it rebuilds the durable position. `None`
    /// before the first synced entry.
    holder: Option<u64>,
    /// The offset at which the lap of the path started: 0, the offset of the trim that
    /// ended the lap before, or the tail of the hide that ended a lap that waited.
    lap: u64,
    /// The most runs that a trim of the lap met.
    peak: u32,
    /// The lap end of the path waits in [`Laps::due`] for a shrink.
    due: bool,
}

impl Log {
    fn new(index: channel::Key, path: Path) -> Self {
        Self {
            index,
            path,
            appended: Position::default(),
            durable: Position::default(),
            runs: VecDeque::new(),
            holder: None,
            lap: 0,
            peak: 0,
            due: false,
        }
    }

    /// Whether an appended entry is not durable. Marks only rise, so equal
    /// positions hold the same entries.
    fn queued(&self) -> bool {
        self.appended != self.durable
    }

    /// Drops the oldest run when it is of the record at `offset`. Returns true when
    /// the lap is due to end: the trim is the first at least `area` after the start
    /// of the lap, and the lap does not wait for a shrink.
    fn trim(&mut self, offset: u64, area: u64) -> bool {
        let met = u32::try_from(self.runs.len()).unwrap_or(u32::MAX);
        if self.runs.front().is_some_and(|run| run.offset == offset) {
            self.runs.pop_front();
        }
        self.peak = self.peak.max(met);
        !self.due && offset >= self.lap.saturating_add(area)
    }

    /// Whether the room is over four times the larger of the runs that the path
    /// holds and the most runs that a trim of the lap met, or over four with none.
    fn oversized(&self) -> bool {
        self.runs.capacity() > self.keep() << 2
    }

    fn keep(&self) -> usize {
        usize::try_from(self.peak)
            .expect("invariant: u32 fits usize")
            .max(self.runs.len())
            .max(1)
    }

    /// Shrinks the room to twice the larger of the runs that the path holds and the
    /// most runs that a trim of the lap met, or to two with none.
    #[cold]
    fn shrink(&mut self) {
        // No deque holds more than `isize::MAX` bytes, so no shift overflows.
        self.runs.shrink_to(self.keep() << 1);
    }

    /// Keeps the lap open until a shrink is left for its end.
    fn wait(&mut self) {
        self.due = true;
    }

    /// Starts the next lap at `offset`.
    fn start(&mut self, offset: u64) {
        self.lap = offset;
        self.peak = u32::try_from(self.runs.len()).unwrap_or(u32::MAX);
        self.due = false;
    }

    /// Moves the durable tail past the entry, and makes the record at `offset` the
    /// holder when the entry gives the durable stamp. Returns true when it adds a run
    /// of the record, which the path has none of.
    ///
    /// # Panics
    ///
    /// When `offset` is below the newest record of the path.
    fn sync(&mut self, header: &Header, offset: u64) -> Result<bool, Fault> {
        let samples = header.samples();
        let (start, _) = self.durable.advance(header.first, samples)?;
        let newest = self.runs.back().map(|run| run.offset).max(self.holder);
        if let Some(newest) = newest {
            assert!(
                newest <= offset,
                "invariant: record {offset} comes after record {newest}"
            );
        }
        if samples.and_then(|samples| samples.last) == self.durable.tail.stamp {
            self.holder = Some(offset);
        }
        if self.runs.back().is_some_and(|run| run.offset == offset) {
            return Ok(false);
        }
        self.runs.push_back(Run { start, offset });
        Ok(true)
    }
}

/// The shrinks of the runs of a path that one [`Logs::hide`] does with no trim.
const SHRINKS: usize = 256;

/// The trims of a [`Logs::hide`] for each shrink that it adds to [`SHRINKS`].
const TRIMS: usize = 4;

/// The items after the reach that one [`Logs::carry`] walks to fill its record, for
/// each tail that the record may hold.
const FILL: usize = 4;

/// The paths whose lap end waits for a shrink, and the shrinks of a [`Logs::hide`]
/// with no trim.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Laps {
    /// The shrinks of a hide with no trim.
    most: usize,
    /// The place of each path whose lap end waits, oldest first. [`Log::due`] marks
    /// each, so a path is here at most once, and it holds at most the number of paths,
    /// in room that [`Laps::add`] takes.
    due: VecDeque<u32>,
}

impl Laps {
    fn new() -> Self {
        Self {
            most: SHRINKS,
            due: VecDeque::new(),
        }
    }

    /// Makes room in `due` for each of `paths` paths, so that no hide allocates.
    fn add(&mut self, paths: usize) {
        let more = paths
            .checked_sub(self.due.len())
            .expect("invariant: a path waits at most once");
        self.due.reserve(more);
    }

    /// The shrinks of a hide that starts now.
    fn budget(&self) -> Budget {
        Budget {
            most: self.most,
            trims: 0,
            used: 0,
        }
    }

    /// Ends the lap of `log`, at place `at`, at a trim at `offset`, and starts the next
    /// there. When the room is over four times the larger of the runs that the path
    /// holds and the most runs that a trim of the lap met, or over four with none, the
    /// end shrinks it. So runs that swing inside a lap take no allocation, and the room
    /// is at most four times the most runs of two laps. When the end needs a shrink,
    /// and a path waits or `budget` has none left, the lap stays open and the path
    /// waits at the back of `due`.
    #[cold]
    fn end(&mut self, budget: &mut Budget, log: &mut Log, at: u32, offset: u64) {
        if log.oversized() {
            if !self.due.is_empty() || !budget.take() {
                log.wait();
                self.due.push_back(at);
                return;
            }
            log.shrink();
        }
        log.start(offset);
    }

    /// Ends the hide whose tail is `offset`: ends there the laps that wait, oldest
    /// first, until the first that needs a shrink when `budget` has none left.
    fn finish(&mut self, budget: &mut Budget, paths: &mut [Log], offset: u64) {
        while let Some(&at) = self.due.front() {
            let log = nth_mut(paths, at);
            if log.oversized() {
                if !budget.take() {
                    return;
                }
                log.shrink();
            }
            log.start(offset);
            self.due.pop_front();
        }
    }
}

/// The shrinks of one [`Logs::hide`]: [`Laps::most`], plus one for each [`TRIMS`]
/// trims.
#[derive(Debug)]
struct Budget {
    most: usize,
    trims: usize,
    used: usize,
}

impl Budget {
    /// Counts `count` trims of the hide.
    fn trimmed(&mut self, count: usize) {
        self.trims = self
            .trims
            .checked_add(count)
            .expect("invariant: the trims of a hide fit in memory");
    }

    /// Takes a shrink. Returns false when none is left.
    fn take(&mut self) -> bool {
        let left = self
            .most
            .checked_add(self.trims / TRIMS)
            .expect("invariant: the shrinks of a hide fit in memory");
        if self.used >= left {
            return false;
        }
        self.used = self
            .used
            .checked_add(1)
            .expect("invariant: a shrink is under the budget");
        true
    }
}

/// The copy of the newest durable entry of one nonzero tag on one path of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Newest {
    /// The offset of the record that holds it.
    pub(crate) offset: u64,
    /// The entry's parts, joined. A later copy of its log and tag reuses it.
    pub(crate) bytes: Vec<u8>,
}

/// Every path the buffer holds, by the node's slot of the index and the path.
/// `append` feeds it with each appended entry; the recovery walk and each sync
/// feed it with each record's entries, in ring order. A trim hides the records
/// before its tail.
#[derive(Clone, Debug)]
pub(crate) struct Logs {
    /// The bytes of the ring: the least span of a lap of a path.
    area: u64,
    /// The lap end of each path.
    laps: Laps,
    /// The place of the log of each path in `paths`.
    places: hash::Map<(Slot, Path), u32>,
    /// The log of each path, in the order that each first came. No log leaves, so a
    /// place in `places`, `items`, and `laps` stays valid. A change that drops a log
    /// updates each.
    paths: Vec<Log>,
    /// The newest durable entry with each nonzero tag, by the log and the tag. Tag
    /// 0 marks samples, which no caller looks up. A change that drops a log drops
    /// its tags in the same place.
    tags: hash::Map<(Slot, Path, NonZeroU8), Newest>,
    /// No read finds a record before this offset.
    hidden: u64,
    /// One item for each run and each carried tail. An item is stale once its path
    /// has a later holder.
    items: Items,
    /// Each item before this number is stale.
    stale: u64,
    /// A [`carry`](Self::carry) has passed each item before this number.
    scanned: u64,
}

/// Compares what the logs hold. `stale` and `scanned` only skip work, so the syncs
/// whose trims moved them and a walk of the same records may differ in them.
#[cfg(test)]
impl PartialEq for Logs {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            area,
            laps,
            places,
            paths,
            tags,
            hidden,
            items,
            stale: _,
            scanned: _,
        } = self;
        *area == other.area
            && *laps == other.laps
            && *places == other.places
            && *paths == other.paths
            && *tags == other.tags
            && *hidden == other.hidden
            && *items == other.items
    }
}

#[cfg(test)]
impl Eq for Logs {}

impl Logs {
    /// The logs of a ring of `area` bytes, with no path.
    pub(crate) fn new(area: u64) -> Self {
        Self {
            area,
            laps: Laps::new(),
            places: hash::Map::default(),
            paths: Vec::new(),
            tags: hash::Map::default(),
            hidden: 0,
            items: Items::default(),
            stale: 0,
            scanned: 0,
        }
    }

    /// Where `path` of the index at `slot` stands with every appended entry:
    /// `Tail::default()` before its first entry.
    pub(crate) fn appended(&self, slot: Slot, path: Path) -> Tail {
        self.log(slot, path).map_or_default(|log| log.appended.tail)
    }

    /// Where `path` of the index at `slot` stands on disk: `Tail::default()`
    /// before its first synced entry.
    pub(crate) fn durable(&self, slot: Slot, path: Path) -> Tail {
        self.log(slot, path).map_or_default(|log| log.durable.tail)
    }

    /// The records that the log of `path` of the index at `slot` keeps, oldest
    /// first.
    #[cfg(test)]
    pub(crate) fn runs(&self, slot: Slot, path: Path) -> impl Iterator<Item = Run> {
        self.log(slot, path)
            .into_iter()
            .flat_map(|log| log.runs.iter().copied())
    }

    /// What a read of `path` of the index at `slot` from `from` finds, among the
    /// records that are not hidden.
    pub(crate) fn find(&self, slot: Slot, path: Path, from: Mark) -> Found {
        let Some(log) = self.log(slot, path) else {
            return Found::End(Mark::at(0));
        };
        // The newest run that starts at or before `from`, else the oldest.
        let chosen = log
            .runs
            .partition_point(|run| run.start <= from)
            .saturating_sub(1);
        let mut runs = log.runs.range(chosen..);
        let end = log.durable.mark();
        match runs.next() {
            Some(run) if runs.next().map_or(end, |next| next.start) > from => {
                Found::Run(log.index, *run)
            }
            _ => Found::End(end),
        }
    }

    /// The newest durable entry with `tag` on `path` of each index whose record is
    /// not hidden, with its slot, in no order.
    pub(crate) fn newest(
        &self,
        path: Path,
        tag: NonZeroU8,
    ) -> impl Iterator<Item = (Slot, &Newest)> {
        self.tags
            .iter()
            .filter_map(move |(&(slot, on, of), newest)| {
                ((on, of) == (path, tag) && newest.offset >= self.hidden)
                    .then_some((slot, newest))
            })
    }

    /// Makes the joined `parts` the newest durable entry of the tag of `header` on
    /// its path of the index at `slot`, in the record at `offset`. It reuses the
    /// bytes of the copy that it replaces, so a copy no larger than that one makes no
    /// heap allocation.
    ///
    /// # Panics
    ///
    /// When the tag of `header` is 0.
    pub(crate) fn copy<P: Deref<Target = [u8]>>(
        &mut self,
        slot: Slot,
        header: &Header,
        offset: u64,
        parts: &[P],
    ) {
        let tag = NonZeroU8::new(header.tag).expect("invariant: a copy has a tag");
        let newest = self
            .tags
            .entry((slot, header.path, tag))
            .or_insert_with(|| Newest {
                offset,
                bytes: Vec::new(),
            });
        newest.offset = offset;
        newest.bytes.clear();
        newest
            .bytes
            .reserve_exact(parts.iter().map(|part| part.len()).sum());
        for part in parts {
            newest.bytes.extend_from_slice(part);
        }
    }

    /// Hides the records before the offset `tail` from each later
    /// [`find`](Self::find) and [`newest`](Self::newest), and drops their runs. It
    /// visits each path once for each run and carried tail of the records that it
    /// hides, and each path whose lap end waits, up to the first that needs a shrink
    /// when none is left. It shrinks the room of the runs of at most [`SHRINKS`]
    /// paths, plus one for each [`TRIMS`] of those runs and carried tails, at their
    /// lap ends, first in, first out. The others wait for a later call: a path waits
    /// less than [`TRIMS`] areas of trims, because a trim of each path falls in each
    /// area.
    ///
    /// # Panics
    ///
    /// When `tail` is before the tail of an earlier call.
    pub(crate) fn hide(&mut self, tail: u64) {
        assert!(
            self.hidden <= tail,
            "invariant: the hidden tail {tail} goes back from {}",
            self.hidden
        );
        let mut budget = self.laps.budget();
        self.hidden = tail;
        while let Some((offset, places)) = self.items.pop(tail) {
            budget.trimmed(places.len());
            for at in places {
                let log = nth_mut(&mut self.paths, at);
                if log.trim(offset, self.area) {
                    self.laps.end(&mut budget, log, at, offset);
                }
            }
        }
        self.laps.finish(&mut budget, &mut self.paths, tail);
    }

    /// The oldest holder of a path, which no trim may pass. `None` when no path has
    /// a synced entry.
    pub(crate) fn kept(&mut self) -> Option<u64> {
        let holding = self
            .items
            .since(self.stale)
            .find(|&(_, offset, at)| nth(&self.paths, at).holder == Some(offset));
        self.stale = holding.map_or_else(|| self.items.added(), |(number, ..)| number);
        holding.map(|(_, offset, _)| offset)
    }

    /// Clears `carried`, then puts in it the appended tail of each path whose holder
    /// is before `reach`, at most `limit`, oldest holder first. When it puts in one,
    /// it fills `carried` up to `limit` with the oldest holders in the next [`FILL`]
    /// `* limit` items after `reach`, so that small carry records merge. The next
    /// call goes on at the first item that this call did not walk. Call it in the
    /// borrow that takes the records of a commit: each appended entry is then in a
    /// taken record, so the tail is the end after those records, and the carry record
    /// goes right after them.
    pub(crate) fn carry(&mut self, reach: u64, limit: usize, carried: &mut Carried) {
        carried.clear();
        let mut walk = limit.saturating_mul(FILL);
        for (number, offset, at) in self.items.since(self.scanned) {
            if offset >= reach {
                let filling = !carried.tails.is_empty();
                let Some(left) = walk.checked_sub(1).filter(|_| filling) else {
                    self.scanned = number;
                    return;
                };
                walk = left;
            }
            let log = nth(&self.paths, at);
            if log.holder != Some(offset) {
                continue;
            }
            if carried.tails.len() == limit {
                self.scanned = number;
                return;
            }
            if carried.tails.is_empty() {
                carried.first = number;
            }
            carried.tails.push(carry::Tail {
                index: log.index,
                path: log.path,
                seq: log.appended.tail.seq,
                given: log.appended.given,
                stamp: log.appended.tail.stamp,
            });
            carried.places.push(at);
        }
        self.scanned = self.items.added();
    }

    /// Takes back the [`carry`](Self::carry) call that filled `carried`, whose carry
    /// record no commit writes, so that the next call gives its tails again.
    /// Clears `carried`.
    pub(crate) fn dropped(&mut self, carried: &mut Carried) {
        if !carried.tails.is_empty() {
            self.scanned = carried.first;
        }
        carried.clear();
    }

    /// Makes the carry record at `offset` the holder of each path of `carried`, once
    /// the records before it are synced. A path with queued entries keeps its
    /// appended tail.
    ///
    /// # Panics
    ///
    /// When a tail is not the durable end of its path, or `offset` is not after the
    /// newest record of the path.
    pub(crate) fn hold(&mut self, carried: &Carried, offset: u64) {
        for (tail, &at) in carried.tails.iter().zip(&carried.places) {
            let log = nth_mut(&mut self.paths, at);
            let end = Position {
                tail: Tail {
                    seq: tail.seq,
                    stamp: tail.stamp,
                },
                given: tail.given,
            };
            assert!(
                log.durable == end,
                "invariant: the carried tail {tail:?} is the durable end {:?}",
                log.durable
            );
            let newest = log.runs.back().map(|run| run.offset).max(log.holder);
            assert!(
                newest < Some(offset),
                "invariant: carry record {offset} comes after record {newest:?}"
            );
            log.holder = Some(offset);
            self.items.push(offset, at);
        }
    }

    /// The log of `path` of the index at `slot`.
    fn log(&self, slot: Slot, path: Path) -> Option<&Log> {
        let &at = self.places.get(&(slot, path))?;
        Some(nth(&self.paths, at))
    }

    /// Adds `log` for `key`, and returns its place in `paths`.
    fn add(&mut self, key: (Slot, Path), log: Log) -> u32 {
        let at = u32::try_from(self.paths.len()).expect("invariant: paths fit in u32");
        self.paths.push(log);
        self.places.insert(key, at);
        self.laps.add(self.paths.len());
        at
    }

    /// Gives a path the tail that the carry record at `offset` holds, and makes the
    /// record its holder. The recovery walk feeds it each carry record, in ring
    /// order. A path that the walk has not met moves to the carried tail and end,
    /// with no run. A path it met keeps its runs, and takes the carried stamp when it
    /// has none: a trim passed the entry with the stamp. A path with entries that
    /// are not durable keeps its appended tail.
    ///
    /// # Errors
    ///
    /// [`Invalid::Carried`] when the walk met the path and the carried end is not
    /// its end, or the path has a stamp and the carried stamp is another.
    /// [`Invalid::Repeated`] when the record at `offset` already gave the path its
    /// tail. Nothing changes.
    ///
    /// # Panics
    ///
    /// When the log of `slot` on the carried path holds another index.
    pub(crate) fn recover(
        &mut self,
        slot: Slot,
        carried: &carry::Tail,
        offset: u64,
    ) -> Result<(), Invalid> {
        let key = (slot, carried.path);
        let Some(&at) = self.places.get(&key) else {
            let mut log = Log::new(carried.index, carried.path);
            log.durable = Position {
                tail: Tail {
                    seq: carried.seq,
                    stamp: carried.stamp,
                },
                given: carried.given,
            };
            log.appended = log.durable;
            log.holder = Some(offset);
            let at = self.add(key, log);
            self.items.push(offset, at);
            return Ok(());
        };
        let log = nth_mut(&mut self.paths, at);
        assert!(
            log.index == carried.index,
            "invariant: slot {} holds index {} and index {}",
            slot.get(),
            log.index,
            carried.index,
        );
        if log.holder == Some(offset) {
            return Err(Invalid::Repeated {
                index: carried.index,
                path: carried.path,
            });
        }
        let end = log.durable.mark();
        let stamp = log.durable.tail.stamp;
        let carried_end = Mark {
            seq: carried.seq,
            given: carried.given,
        };
        if carried_end != end || stamp.is_some_and(|_| carried.stamp != stamp) {
            return Err(Invalid::Carried {
                carried: *carried,
                end,
                stamp,
            });
        }
        let queued = log.queued();
        log.durable.tail.stamp = carried.stamp;
        if !queued {
            log.appended = log.durable;
        }
        log.holder = Some(offset);
        self.items.push(offset, at);
        Ok(())
    }

    /// Moves the appended tail of `path` of `index` past the entry of `samples` at
    /// seq `first`, as [`Position::advance`], and gives the kind of its header. The
    /// kind of an entry with no samples counts the entries with no samples at its
    /// seq on the path, it included.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Position::advance`]. Nothing changes.
    pub(crate) fn append(
        &mut self,
        slot: Slot,
        index: channel::Key,
        path: Path,
        first: u64,
        samples: Option<Samples>,
    ) -> Result<Kind, Invalid> {
        let (kind, _) = self.change(slot, index, path, |log| {
            Ok(log.appended.advance(first, samples)?.1)
        })?;
        Ok(kind)
    }

    /// Moves the durable tail of the header's path past the entry, as
    /// [`Position::advance`], and adds the record at `offset` to the path's runs when
    /// it is not the newest. The record becomes the path's holder when the entry gives
    /// the durable stamp.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Position::advance`]. Nothing changes.
    ///
    /// # Panics
    ///
    /// When `offset` is below the newest run or the holder of the path, or when the
    /// slot holds another index.
    pub(crate) fn sync(
        &mut self,
        slot: Slot,
        header: &Header,
        offset: u64,
    ) -> Result<(), Invalid> {
        let (added, at) = self.change(slot, header.index, header.path, |log| {
            log.sync(header, offset)
        })?;
        if added {
            self.items.push(offset, at);
        }
        Ok(())
    }

    /// Feeds the recovery walk's entry `header` of the record at `offset`, as
    /// appended and synced. On a path that the walk has not met, an entry with no
    /// samples starts after the others that its count gives: a trim passed them.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`append`](Self::append), and [`Invalid::Counted`] when the
    /// walk met the path and the entry's count is another than the walk's. Nothing
    /// changes.
    ///
    /// # Panics
    ///
    /// As [`sync`](Self::sync), and when the slot holds another index.
    pub(crate) fn walk(
        &mut self,
        slot: Slot,
        header: &Header,
        offset: u64,
    ) -> Result<(), Invalid> {
        let key = (slot, header.path);
        if let Kind::Record(given) = header.kind
            && !self.places.contains_key(&key)
        {
            let mut log = Log::new(header.index, header.path);
            log.durable = Position {
                tail: Tail {
                    seq: header.first,
                    stamp: None,
                },
                given: given.get() - 1,
            };
            log.appended = log.durable;
            self.add(key, log);
        }
        self.change(slot, header.index, header.path, |log| {
            let mut appended = log.appended;
            let (_, walked) = appended.advance(header.first, header.samples())?;
            if let (Kind::Record(given), Kind::Record(walked)) = (header.kind, walked)
                && given != walked
            {
                return Err(Fault::Counted {
                    seq: header.first,
                    given: given.get(),
                    walked: walked.get(),
                });
            }
            log.appended = appended;
            Ok(())
        })?;
        self.sync(slot, header, offset)
    }

    /// Applies `change` to the log of `slot` on `path`, which starts empty when the
    /// path is new, and gives the place of the log. A change that fails on a new path
    /// adds nothing.
    ///
    /// # Errors
    ///
    /// The [`Invalid`] of the [`Fault`] that `change` gives, on `path` of `index`.
    ///
    /// # Panics
    ///
    /// When the log of `slot` on `path` holds another index than `index`.
    fn change<T>(
        &mut self,
        slot: Slot,
        index: channel::Key,
        path: Path,
        change: impl FnOnce(&mut Log) -> Result<T, Fault>,
    ) -> Result<(T, u32), Invalid> {
        let invalid = |fault: Fault| fault.on(index, path);
        let key = (slot, path);
        if let Some(&at) = self.places.get(&key) {
            let log = nth_mut(&mut self.paths, at);
            assert!(
                log.index == index,
                "invariant: slot {} holds index {} and index {}",
                slot.get(),
                log.index,
                index,
            );
            return Ok((change(log).map_err(invalid)?, at));
        }
        let mut log = Log::new(index, path);
        let changed = change(&mut log).map_err(invalid)?;
        Ok((changed, self.add(key, log)))
    }
}

/// The tails of one carry record, with the place of the log of each, oldest holder
/// first.
#[derive(Clone, Debug, Default)]
pub(crate) struct Carried {
    tails: Vec<carry::Tail>,
    places: Vec<u32>,
    /// The number of the item of the first tail.
    first: u64,
}

impl Carried {
    /// Room for `tails` tails, so that a carry of at most that many allocates
    /// nothing.
    #[cfg(feature = "sim")]
    pub(crate) fn with_capacity(tails: usize) -> Self {
        Self {
            tails: Vec::with_capacity(tails),
            places: Vec::with_capacity(tails),
            first: 0,
        }
    }

    /// The tails, in the order of the record body.
    pub(crate) fn tails(&self) -> &[carry::Tail] {
        &self.tails
    }

    /// Drops the tails and keeps the capacity.
    pub(crate) fn clear(&mut self) {
        self.tails.clear();
        self.places.clear();
    }
}

/// The log at `at` in `logs`.
fn nth(logs: &[Log], at: u32) -> &Log {
    let at = usize::try_from(at).expect("invariant: u32 fits usize");
    logs.get(at).expect("invariant: a place has its log")
}

/// The log at `at` in `logs`.
fn nth_mut(logs: &mut [Log], at: u32) -> &mut Log {
    let at = usize::try_from(at).expect("invariant: u32 fits usize");
    logs.get_mut(at).expect("invariant: a place has its log")
}

/// The place in a `Vec` of logs of each item, grouped by record in ring order. Items
/// are numbered from 0 in the order that they came, and no number is used twice.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Items {
    /// The offset of each record with an item, and the number after its last item.
    records: VecDeque<(u64, u64)>,
    /// The place of each item.
    places: VecDeque<u32>,
    /// The number of the front item.
    base: u64,
}

impl Items {
    /// Adds an item for the log at `at` in the record at `offset`, which is at or
    /// after the record of each item.
    fn push(&mut self, offset: u64, at: u32) {
        let next = self.added().checked_add(1).expect("invariant: numbers fit");
        match self.records.back_mut() {
            Some((last, end)) if *last == offset => *end = next,
            _ => self.records.push_back((offset, next)),
        }
        self.places.push_back(at);
    }

    /// The number of the next item that `push` adds.
    fn added(&self) -> u64 {
        self.records.back().map_or(self.base, |&(_, end)| end)
    }

    /// Removes the front record when it is before `tail`, and gives its offset and
    /// the places of its items.
    fn pop(&mut self, tail: u64) -> Option<(u64, Drain<'_, u32>)> {
        let &(offset, end) =
            self.records.front().filter(|&&(offset, _)| offset < tail)?;
        self.records.pop_front();
        let count = self.index(end);
        self.base = end;
        Some((offset, self.places.drain(..count)))
    }

    /// Each item from the number `from`, or from the front item when it is past, with
    /// its number and the offset of its record, oldest first.
    fn since(&self, from: u64) -> impl Iterator<Item = (u64, u64, u32)> + '_ {
        let mut start = from.max(self.base);
        let first = self.records.partition_point(|&(_, end)| end <= start);
        let skip = self.index(start);
        let numbers = self.records.range(first..).flat_map(move |&(offset, end)| {
            let numbers = start..end;
            start = end;
            numbers.map(move |number| (number, offset))
        });
        numbers
            .zip(self.places.range(skip..))
            .map(|((number, offset), &at)| (number, offset, at))
    }

    /// The index in `places` of the item numbered `number`, at or after the front.
    fn index(&self, number: u64) -> usize {
        number
            .checked_sub(self.base)
            .and_then(|index| usize::try_from(index).ok())
            .expect("invariant: a number at or after the front fits")
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use proptest::prelude::*;

    /// The area of the logs of each test: 64 records of a block.
    const AREA: u64 = 64 * 4096;

    fn slot(value: u32) -> Slot {
        Slot::new(value)
    }

    fn header(
        index: u32,
        path: Path,
        first: u64,
        len: u32,
        last: Option<i64>,
    ) -> Header {
        Header {
            index: channel::Key::from_u128(u128::from(index)),
            path,
            first,
            stored_at: Stamp::from_nanos(7),
            kind: kind(len, last),
            tag: 0,
            bytes: 0,
        }
    }

    /// The kind of an entry of `len` samples: an entry with no samples counts 1
    /// until [`append`] counts it.
    fn kind(len: u32, last: Option<i64>) -> Kind {
        match NonZeroU32::new(len) {
            Some(len) => Kind::Samples(Samples {
                len,
                last: last.map(Stamp::from_nanos),
            }),
            None => Kind::Record(NonZeroU64::MIN),
        }
    }

    /// Appends the entry of `header` on `slot`, as the buffer does, and gives
    /// `header` the kind that the logs give.
    fn append(logs: &mut Logs, slot: Slot, header: &mut Header) -> Result<(), Invalid> {
        header.kind = logs.append(
            slot,
            header.index,
            header.path,
            header.first,
            header.samples(),
        )?;
        Ok(())
    }

    fn run(seq: u64, given: u64, offset: u64) -> Run {
        Run {
            start: Mark { seq, given },
            offset,
        }
    }

    fn mark(seq: u64, given: u64) -> Mark {
        Mark { seq, given }
    }

    /// One entry of a model log: which path it is on, how far past the tail it
    /// starts, and what it carries.
    #[derive(Clone, Debug)]
    struct Next {
        index: u32,
        path: Path,
        skip: u64,
        len: u32,
        last: Option<i64>,
    }

    fn next() -> impl Strategy<Value = Next> {
        (
            0..3u32,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..3u64,
            0..5u32,
            prop::option::of(any::<i64>()),
        )
            .prop_map(|(index, path, skip, len, last)| Next {
                index,
                path,
                skip,
                len,
                last,
            })
    }

    /// The entries of the model, each with the offset of its record.
    type Fed = [(Slot, Header, u64)];

    /// The entries of one path, in log order, with their slot and offset.
    fn on_fed(
        fed: &Fed,
        slot: Slot,
        path: Path,
    ) -> impl Iterator<Item = &(Slot, Header, u64)> {
        fed.iter()
            .filter(move |(at, header, _)| (*at, header.path) == (slot, path))
    }

    /// The entries of one path, in log order.
    fn on(fed: &Fed, slot: Slot, path: Path) -> impl Iterator<Item = &Header> {
        on_fed(fed, slot, path).map(|(_, header, _)| header)
    }

    /// The tail the model expects for one path: the seq past its newest entry,
    /// and the `last` of its newest entry that has one.
    fn tail(fed: &Fed, slot: Slot, path: Path) -> Tail {
        Tail {
            seq: on(fed, slot, path)
                .map(|header| header.first + u64::from(header.len()))
                .max()
                .unwrap_or(0),
            stamp: on(fed, slot, path)
                .filter_map(|header| header.samples()?.last)
                .last(),
        }
    }

    /// The runs the model expects for one path: one per record that holds an
    /// entry of it, with the mark before its first entry there. Also gives the
    /// mark before each entry of the path, in order.
    fn expected(fed: &Fed, slot: Slot, path: Path) -> (Vec<Run>, Vec<Mark>) {
        let mut runs: Vec<Run> = Vec::new();
        let mut marks = Vec::new();
        let mut at = Mark::at(0);
        for (slot_of, header, offset) in fed {
            if (*slot_of, header.path) != (slot, path) {
                continue;
            }
            if runs.last().is_none_or(|run| run.offset != *offset) {
                runs.push(Run {
                    start: at,
                    offset: *offset,
                });
            }
            marks.push(at);
            at = at.after(header.first, header.len());
        }
        (runs, marks)
    }

    proptest! {
        /// Each record's entries are appended, then the record is synced. The
        /// appended tail runs ahead of the durable tail by the open record, and
        /// the runs follow the model.
        #[test]
        fn follows_a_model_per_path(
            records in prop::collection::vec(prop::collection::vec(next(), 1..5), 0..12),
        ) {
            let mut logs = Logs::new(AREA);
            let mut fed: Vec<(Slot, Header, u64)> = Vec::new();
            for (number, record) in records.iter().enumerate() {
                let offset = 4096 * (u64::try_from(number).expect("small") + 1);
                let synced = fed.len();
                for next in record {
                    let at = slot(next.index);
                    let first = logs.appended(at, next.path).seq + next.skip;
                    let mut header = header(next.index, next.path, first, next.len, next.last);
                    append(&mut logs, at, &mut header).expect("appends");
                    fed.push((at, header, offset));
                    let (durable, _) = fed.split_at(synced);
                    prop_assert_eq!(logs.appended(at, next.path), tail(&fed, at, next.path));
                    prop_assert_eq!(logs.durable(at, next.path), tail(durable, at, next.path));
                }
                let (_, open) = fed.split_at(synced);
                for (at, header, offset) in open {
                    prop_assert_eq!(logs.sync(*at, header, *offset), Ok(()));
                }
            }
            for index in 0..3 {
                for path in [Path::Live, Path::Backfill] {
                    let runs: Vec<Run> = logs.runs(slot(index), path).collect();
                    let (expected, marks) = expected(&fed, slot(index), path);
                    prop_assert_eq!(&runs, &expected);
                    prop_assert_eq!(logs.appended(slot(index), path), tail(&fed, slot(index), path));
                    prop_assert_eq!(logs.durable(slot(index), path), tail(&fed, slot(index), path));
                    let key = channel::Key::from_u128(u128::from(index));
                    let mut end = Mark::at(0);
                    let fed_on = on_fed(&fed, slot(index), path);
                    for ((_, header, offset), before) in fed_on.zip(&marks) {
                        let run = runs.iter().rev().find(|run| run.offset == *offset);
                        let found = Found::Run(key, *run.expect("an entry has a run"));
                        prop_assert_eq!(logs.find(slot(index), path, *before), found);
                        end = before.after(header.first, header.len());
                    }
                    prop_assert_eq!(logs.find(slot(index), path, end), Found::End(end));
                }
            }
        }

        /// A trim after any record hides the records before its tail and drops
        /// their runs. Each read then finds the oldest record left with an entry
        /// past its mark, else the durable end.
        #[test]
        fn finds_what_the_trims_left(
            records in prop::collection::vec(
                prop::collection::vec(next(), 1..5),
                1..12,
            ),
            trims in prop::collection::vec(prop::option::of(0..14u64), 12),
        ) {
            let mut logs = Logs::new(AREA);
            let mut fed: Vec<(Slot, Header, u64)> = Vec::new();
            let mut tail = 0;
            for ((record, trim), number) in records.iter().zip(&trims).zip(1..) {
                let offset = 4096 * number;
                for next in record {
                    let at = slot(next.index);
                    let first = logs.durable(at, next.path).seq + next.skip;
                    let header =
                        header(next.index, next.path, first, next.len, next.last);
                    prop_assert_eq!(logs.sync(at, &header, offset), Ok(()));
                    fed.push((at, header, offset));
                }
                if let Some(trim) = trim {
                    tail = tail.max(4096 * trim.min(&(number + 1)));
                    logs.hide(tail);
                }
            }
            for index in 0..3 {
                for path in [Path::Live, Path::Backfill] {
                    let at = slot(index);
                    let key = channel::Key::from_u128(u128::from(index));
                    let (runs, mut marks) = expected(&fed, at, path);
                    let end = on(&fed, at, path)
                        .zip(&marks)
                        .last()
                        .map_or(Mark::at(0), |(header, before)| {
                            before.after(header.first, header.len())
                        });
                    marks.push(end);
                    // No call of `Logs` shows the drop, so the model reads the runs.
                    let kept: Vec<Run> = logs.runs(at, path).collect();
                    let expected: Vec<Run> =
                        runs.iter().copied().filter(|run| run.offset >= tail).collect();
                    prop_assert_eq!(kept, expected);
                    for from in &marks {
                        let found = on_fed(&fed, at, path)
                            .zip(marks.iter().skip(1))
                            .find(|((_, _, offset), after)| {
                                *offset >= tail && *after > from
                            })
                            .map_or(Found::End(end), |((_, _, offset), _)| {
                                let run =
                                    runs.iter().rev().find(|run| run.offset == *offset);
                                Found::Run(key, *run.expect("each entry has a run"))
                            });
                        prop_assert_eq!(logs.find(at, path, *from), found);
                    }
                }
            }
        }

        /// Each record's entries are appended, a commit asks for the idle tails up to
        /// a reach, a carry record of those tails may come after the record of the
        /// entries, and a trim may pass the records before the oldest holder. Each
        /// tail comes once for its holder, oldest holder first, with the path's
        /// appended end. A call with room left has given each path whose holder is
        /// before the reach.
        #[test]
        fn gives_each_idle_tail_once_and_keeps_the_oldest_holder(
            records in prop::collection::vec(
                (
                    prop::collection::vec(next(), 0..4),
                    0..14u64,
                    0..4usize,
                    prop::option::of(0..14u64),
                    any::<bool>(),
                ),
                1..12,
            ),
        ) {
            let mut logs = Logs::new(AREA);
            let mut model = Model::default();
            let numbered = records.iter().zip(1..);
            for ((record, reach, limit, trim, carries), number) in numbered {
                let synced_at = 8192 * number;
                let carried_at = synced_at + 4096;
                let mut queued = Vec::new();
                for next in record {
                    let at = slot(next.index);
                    let first = logs.appended(at, next.path).seq + next.skip;
                    let mut header =
                        header(next.index, next.path, first, next.len, next.last);
                    append(&mut logs, at, &mut header).expect("appends");
                    queued.push((next.index, header));
                }
                let reach = 8192 * *reach.min(&number);
                let mut carried = Carried::default();
                logs.carry(reach, *limit, &mut carried);
                let holders = model.holders();
                for (index, header) in &queued {
                    prop_assert_eq!(logs.sync(slot(*index), header, synced_at), Ok(()));
                    model.sync(*index, header, synced_at);
                }
                model.check(carried.tails(), reach, *limit, &holders)?;
                if *carries {
                    logs.hold(&carried, carried_at);
                    for tail in carried.tails() {
                        model.carry(tail, carried_at);
                    }
                }
                let kept = model.kept();
                prop_assert_eq!(logs.kept(), kept);
                if let Some(trim) = trim {
                    let bound = kept.unwrap_or(synced_at + 4096);
                    model.hidden = model.hidden.max((8192 * trim).min(bound));
                    logs.hide(model.hidden);
                    prop_assert_eq!(
                        logs.kept(),
                        kept,
                        "a trim to the oldest holder keeps it"
                    );
                }
            }
        }
    }

    /// What the logs hold for each path, by index and path: the holder, the
    /// durable end, and the stamp. Also the holders whose tail `carry` gave.
    #[derive(Default)]
    struct Model {
        paths: hash::Map<(u32, Path), (u64, Mark, Option<Stamp>)>,
        given: hash::Set<(u32, Path, u64)>,
        hidden: u64,
    }

    impl Model {
        /// The holder of each path.
        fn holders(&self) -> hash::Map<(u32, Path), u64> {
            self.paths
                .iter()
                .map(|(&key, &(holder, ..))| (key, holder))
                .collect()
        }

        /// Checks the tails of one call of `carry` up to `reach` with `limit`, made
        /// when each path had its holder in `holders`, after the sync of the
        /// entries that it saw appended. Tails past `reach` only fill a call that
        /// gave a tail before it, and a fill stops after a bounded walk, so only the
        /// holders before the newest tail must all be given.
        fn check(
            &mut self,
            tails: &[carry::Tail],
            reach: u64,
            limit: usize,
            holders: &hash::Map<(u32, Path), u64>,
        ) -> Result<(), TestCaseError> {
            prop_assert!(tails.len() <= limit);
            if let Some(first) = tails.first() {
                let holder = holders[&(index_of(first.index), first.path)];
                prop_assert!(holder < reach, "first holder {}", holder);
            }
            let mut previous = 0;
            for tail in tails {
                let key = (index_of(tail.index), tail.path);
                let holder = holders[&key];
                let (_, end, stamp) = self.paths[&key];
                prop_assert!(self.hidden <= holder, "holder {}", holder);
                prop_assert!(previous <= holder, "oldest holder first");
                previous = holder;
                prop_assert_eq!(
                    (tail.seq, tail.given, tail.stamp),
                    (end.seq, end.given, stamp)
                );
                let first = self.given.insert((key.0, key.1, holder));
                prop_assert!(first, "given twice");
            }
            let newest = tails
                .last()
                .map_or(0, |tail| holders[&(index_of(tail.index), tail.path)]);
            let end = if tails.len() < limit {
                reach.max(newest)
            } else {
                newest
            };
            let left = holders.iter().find(|&(&(index, path), &holder)| {
                holder < end && !self.given.contains(&(index, path, holder))
            });
            prop_assert!(left.is_none(), "{:?} is left out", left);
            Ok(())
        }

        /// Takes a sync of `header` of `index` in the record at `offset`. The record
        /// becomes the holder when the entry alone gives the end and the stamp.
        fn sync(&mut self, index: u32, header: &Header, offset: u64) {
            let key = (index, header.path);
            let (holder, end, stamp) =
                self.paths
                    .get(&key)
                    .copied()
                    .unwrap_or((offset, Mark::at(0), None));
            let end = end.after(header.first, header.len());
            let last = header.samples().and_then(|samples| samples.last);
            let stamp = last.or(stamp);
            let holder = if last == stamp { offset } else { holder };
            self.paths.insert(key, (holder, end, stamp));
        }

        /// Takes the carry record at `offset` of the path of `tail`.
        fn carry(&mut self, tail: &carry::Tail, offset: u64) {
            let key = (index_of(tail.index), tail.path);
            self.paths.get_mut(&key).expect("a given tail").0 = offset;
        }

        fn kept(&self) -> Option<u64> {
            self.paths.values().map(|&(holder, ..)| holder).min()
        }
    }

    /// The index that `next` fed as `key`.
    fn index_of(key: channel::Key) -> u32 {
        (0..3u32)
            .find(|&i| channel::Key::from_u128(u128::from(i)) == key)
            .expect("a fed index")
    }

    #[test]
    fn a_new_path_stands_at_seq_zero_with_no_stamp() {
        let logs = Logs::new(AREA);
        assert_eq!(logs.appended(slot(1), Path::Live), Tail::default());
        assert_eq!(logs.durable(slot(1), Path::Live), Tail::default());
        assert_eq!(logs.runs(slot(1), Path::Live).count(), 0);
    }

    #[test]
    fn each_path_of_an_index_has_its_own_tail() {
        let mut logs = Logs::new(AREA);
        append(
            &mut logs,
            slot(1),
            &mut header(1, Path::Live, 0, 3, Some(30)),
        )
        .expect("appends");
        append(
            &mut logs,
            slot(1),
            &mut header(1, Path::Backfill, 0, 1, Some(10)),
        )
        .expect("appends");
        append(&mut logs, slot(2), &mut header(2, Path::Live, 0, 5, None))
            .expect("appends");
        let live = Tail {
            seq: 3,
            stamp: Some(Stamp::from_nanos(30)),
        };
        let backfill = Tail {
            seq: 1,
            stamp: Some(Stamp::from_nanos(10)),
        };
        assert_eq!(logs.appended(slot(1), Path::Live), live);
        assert_eq!(logs.appended(slot(1), Path::Backfill), backfill);
        assert_eq!(
            logs.appended(slot(2), Path::Live),
            Tail {
                seq: 5,
                stamp: None
            }
        );
        assert_eq!(logs.appended(slot(2), Path::Backfill), Tail::default());
        assert_eq!(logs.durable(slot(1), Path::Live), Tail::default());
    }

    #[test]
    fn a_skip_ahead_moves_the_seq_past_the_gap() {
        let mut logs = Logs::new(AREA);
        append(
            &mut logs,
            slot(1),
            &mut header(1, Path::Live, 0, 2, Some(2)),
        )
        .expect("appends");
        append(
            &mut logs,
            slot(1),
            &mut header(1, Path::Live, 10, 1, Some(11)),
        )
        .expect("appends");
        let expected = Tail {
            seq: 11,
            stamp: Some(Stamp::from_nanos(11)),
        };
        assert_eq!(logs.appended(slot(1), Path::Live), expected);
    }

    #[test]
    fn an_entry_with_no_last_stamp_keeps_the_stamp() {
        let mut logs = Logs::new(AREA);
        append(
            &mut logs,
            slot(1),
            &mut header(1, Path::Live, 0, 2, Some(2)),
        )
        .expect("appends");
        append(&mut logs, slot(1), &mut header(1, Path::Live, 2, 0, None))
            .expect("appends");
        let expected = Tail {
            seq: 2,
            stamp: Some(Stamp::from_nanos(2)),
        };
        assert_eq!(logs.appended(slot(1), Path::Live), expected);
    }

    #[test]
    fn an_entry_below_the_tail_is_invalid_and_leaves_the_tail() {
        let mut logs = Logs::new(AREA);
        append(&mut logs, slot(1), &mut header(1, Path::Live, 0, 3, None))
            .expect("appends");
        let invalid = Invalid::Below {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: 2,
            tail: 3,
        };
        assert_eq!(
            append(
                &mut logs,
                slot(1),
                &mut header(1, Path::Live, 2, 1, Some(9))
            ),
            Err(invalid)
        );
        assert_eq!(
            invalid.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live \
             starts at 2, below the tail 3"
        );
        assert_eq!(
            logs.appended(slot(1), Path::Live),
            Tail {
                seq: 3,
                stamp: None
            }
        );
    }

    #[test]
    fn an_entry_past_the_last_seq_is_invalid_and_leaves_the_tail() {
        let mut logs = Logs::new(AREA);
        let invalid = Invalid::Past {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: u64::MAX,
            len: 1,
        };
        assert_eq!(
            append(
                &mut logs,
                slot(1),
                &mut header(1, Path::Live, u64::MAX, 1, Some(9))
            ),
            Err(invalid)
        );
        assert_eq!(
            invalid.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live \
             starts at 18446744073709551615 with 1 samples, past the last seq"
        );
        assert_eq!(logs, Logs::new(AREA));
    }

    #[test]
    fn newest_gives_the_last_copy_of_each_index_until_a_hide() {
        let mut logs = Logs::new(AREA);
        let tagged = |index, path, tag| Header {
            tag,
            ..header(index, path, 0, 2, None)
        };
        let bytes = |bytes: &[u8]| bytes.to_vec();
        logs.copy(
            slot(1),
            &tagged(1, Path::Live, 2),
            4096,
            &[b"old".as_slice()],
        );
        logs.copy(
            slot(1),
            &tagged(1, Path::Live, 2),
            8192,
            &[b"ne".as_slice(), b"w"],
        );
        logs.copy(
            slot(2),
            &tagged(2, Path::Live, 2),
            4096,
            &[b"two".as_slice()],
        );
        logs.copy(
            slot(1),
            &tagged(1, Path::Live, 3),
            4096,
            &[b"tag 3".as_slice()],
        );
        logs.copy(
            slot(1),
            &tagged(1, Path::Backfill, 2),
            4096,
            &[b"back".as_slice()],
        );
        let tag = NonZeroU8::new(2).expect("not 0");
        let newest = |logs: &Logs| {
            let mut newest: Vec<(Slot, Newest)> = logs
                .newest(Path::Live, tag)
                .map(|(slot, newest)| (slot, newest.clone()))
                .collect();
            newest.sort_unstable_by_key(|&(slot, _)| slot);
            newest
        };
        let new = Newest {
            offset: 8192,
            bytes: bytes(b"new"),
        };
        let two = Newest {
            offset: 4096,
            bytes: bytes(b"two"),
        };
        assert_eq!(newest(&logs), [(slot(1), new.clone()), (slot(2), two)]);
        logs.hide(8192);
        assert_eq!(newest(&logs), [(slot(1), new)]);
    }

    #[test]
    fn a_copy_replaces_the_copy_of_its_path_and_tag() {
        let mut logs = Logs::new(AREA);
        let header = Header {
            tag: 2,
            ..header(1, Path::Live, 0, 2, None)
        };
        let tag = NonZeroU8::new(2).expect("not 0");
        logs.copy(slot(1), &header, 4096, &[b"long".as_slice(), b" entry"]);
        logs.copy(slot(1), &header, 8192, &[b"short".as_slice()]);
        let (_, newest) = logs.newest(Path::Live, tag).next().expect("a copy");
        let kept = (newest.bytes.as_slice(), newest.offset);
        assert_eq!(kept, (b"short".as_slice(), 8192));
    }

    #[test]
    fn sync_makes_no_copy() {
        let mut logs = Logs::new(AREA);
        let header = Header {
            tag: 2,
            ..header(1, Path::Live, 0, 2, None)
        };
        logs.sync(slot(1), &header, 4096).expect("syncs");
        let tag = NonZeroU8::new(2).expect("not 0");
        assert_eq!(logs.newest(Path::Live, tag).count(), 0);
    }

    #[test]
    #[should_panic(expected = "invariant: a copy has a tag")]
    fn a_copy_of_tag_0_is_a_broken_invariant() {
        let mut logs = Logs::new(AREA);
        let header = header(1, Path::Live, 0, 2, None);
        logs.copy::<&[u8]>(slot(1), &header, 4096, &[]);
    }

    #[test]
    fn a_record_with_two_entries_of_a_path_makes_one_run() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 2, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 2, 2, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Backfill, 0, 1, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 4, 0, None), 8192)
            .expect("syncs");
        let live: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(live, [run(0, 0, 4096), run(4, 0, 8192)]);
        let backfill: Vec<Run> = logs.runs(slot(1), Path::Backfill).collect();
        assert_eq!(backfill, [run(0, 0, 4096)]);
        assert_eq!(logs.durable(slot(1), Path::Live).seq, 4);
        assert_eq!(logs.appended(slot(1), Path::Live), Tail::default());
        assert_eq!(logs.runs(slot(2), Path::Live).count(), 0);
        assert_eq!(logs.durable(slot(2), Path::Live), Tail::default());
    }

    #[test]
    fn an_invalid_sync_changes_nothing() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 4096)
            .expect("syncs");
        let before = logs.clone();
        let invalid = Invalid::Below {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: 2,
            tail: 3,
        };
        assert_eq!(
            logs.sync(slot(1), &header(1, Path::Live, 2, 1, None), 8192),
            Err(invalid)
        );
        assert_eq!(logs, before);
    }

    #[test]
    fn an_invalid_sync_of_a_new_path_changes_nothing() {
        let mut logs = Logs::new(AREA);
        let invalid = Invalid::Past {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: u64::MAX,
            len: 1,
        };
        assert_eq!(
            logs.sync(slot(1), &header(1, Path::Live, u64::MAX, 1, None), 4096),
            Err(invalid)
        );
        assert_eq!(logs, Logs::new(AREA));
    }

    #[test]
    #[should_panic(expected = "invariant: record 4096 comes after record 8192")]
    fn a_record_before_the_newest_run_is_a_broken_invariant() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 8192)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 3, 1, None), 4096)
            .expect("the invariant panics first");
    }

    /// The holder stays at 4096, since the entries with no samples at 12288 do not
    /// alone give the stamp, so the newest run checks the order.
    #[test]
    #[should_panic(expected = "invariant: record 8192 comes after record 12288")]
    fn a_record_before_a_run_newer_than_the_holder_is_a_broken_invariant() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, Some(30)), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 3, 0, None), 12288)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 3, 0, None), 8192)
            .expect("the invariant panics first");
    }

    #[test]
    #[should_panic(expected = "invariant: slot 1 holds index \
        00000000-0000-0000-0000-000000000001 and index \
        00000000-0000-0000-0000-000000000002")]
    fn a_slot_with_a_second_index_is_a_broken_invariant() {
        let mut logs = Logs::new(AREA);
        append(&mut logs, slot(1), &mut header(1, Path::Live, 0, 3, None))
            .expect("appends");
        append(&mut logs, slot(1), &mut header(2, Path::Live, 3, 2, None))
            .expect("the invariant panics first");
    }

    #[test]
    fn a_mark_moves_past_an_entry_as_a_read_does() {
        assert_eq!(mark(0, 0).after(0, 3), mark(3, 0));
        assert_eq!(mark(1, 0).after(0, 3), mark(3, 0), "inside the entry");
        assert_eq!(mark(3, 0).after(5, 2), mark(7, 0), "a skip ahead");
        assert_eq!(mark(3, 0).after(3, 0), mark(3, 1), "no samples");
        assert_eq!(mark(3, 1).after(3, 0), mark(3, 2), "no samples again");
        assert_eq!(
            mark(3, 2).after(5, 0),
            mark(5, 1),
            "no samples after a skip"
        );
        assert!(mark(3, 0) < mark(3, 1) && mark(3, 1) < mark(4, 0));
    }

    #[test]
    #[should_panic(expected = "invariant: the tail checked the entry")]
    fn a_mark_past_the_last_seq_is_a_broken_invariant() {
        let _: Mark = mark(0, 0).after(u64::MAX, 1);
    }

    /// Record 1 holds [2, 5) and an entry with no samples at 5; record 2 starts at
    /// 5. A read from 5 starts in record 1, for the entry with no samples.
    #[test]
    fn a_read_from_the_seq_an_empty_entry_stands_at_starts_in_its_record() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 2, 3, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 5, 0, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 5, 2, None), 8192)
            .expect("syncs");
        let key = channel::Key::from_u128(1);
        let first = Found::Run(key, run(0, 0, 4096));
        let second = Found::Run(key, run(5, 1, 8192));
        assert_eq!(logs.find(slot(1), Path::Live, mark(0, 0)), first);
        assert_eq!(logs.find(slot(1), Path::Live, mark(3, 0)), first);
        assert_eq!(logs.find(slot(1), Path::Live, mark(5, 0)), first);
        assert_eq!(logs.find(slot(1), Path::Live, mark(5, 1)), second);
        assert_eq!(logs.find(slot(1), Path::Live, mark(6, 0)), second);
        let end = Found::End(mark(7, 0));
        assert_eq!(logs.find(slot(1), Path::Live, mark(7, 0)), end);
        assert_eq!(logs.find(slot(1), Path::Live, mark(7, 3)), end);
        let new = Found::End(mark(0, 0));
        assert_eq!(logs.find(slot(1), Path::Backfill, mark(0, 0)), new);
        assert_eq!(logs.find(slot(2), Path::Live, mark(0, 0)), new);
        assert_eq!(logs.find(slot(2), Path::Live, mark(1, 0)), new);
    }

    /// Records at 4096, 8192, and 12288 hold [0, 3), [3, 6), and [6, 9) of one
    /// path, and the record at 4096 also holds [0, 2) of a second path.
    fn three_records() -> Logs {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(2), &header(2, Path::Live, 0, 2, None), 4096)
            .expect("syncs");
        for record in 0..3 {
            let header = header(1, Path::Live, 3 * record, 3, None);
            logs.sync(slot(1), &header, 4096 * (record + 1))
                .expect("syncs");
        }
        logs
    }

    #[test]
    fn a_read_finds_the_oldest_record_that_a_trim_left() {
        let mut logs = three_records();
        let key = channel::Key::from_u128(1);
        logs.hide(4096);
        assert_eq!(
            logs.find(slot(1), Path::Live, mark(0, 0)),
            Found::Run(key, run(0, 0, 4096)),
            "a trim keeps the record at its tail"
        );
        logs.hide(8192);
        let second = Found::Run(key, run(3, 0, 8192));
        assert_eq!(logs.find(slot(1), Path::Live, mark(0, 0)), second);
        assert_eq!(logs.find(slot(1), Path::Live, mark(2, 0)), second);
        assert_eq!(logs.find(slot(1), Path::Live, mark(4, 0)), second);
        assert_eq!(
            logs.find(slot(1), Path::Live, mark(6, 0)),
            Found::Run(key, run(6, 0, 12288))
        );
        assert_eq!(
            logs.find(slot(1), Path::Live, mark(9, 0)),
            Found::End(mark(9, 0))
        );
    }

    #[test]
    fn a_read_of_a_path_with_no_record_left_finds_its_durable_end() {
        let mut logs = three_records();
        logs.hide(8192);
        let end = Found::End(mark(2, 0));
        assert_eq!(logs.find(slot(2), Path::Live, mark(0, 0)), end);
        assert_eq!(logs.find(slot(2), Path::Live, mark(1, 0)), end);
        assert_eq!(logs.find(slot(2), Path::Live, mark(2, 0)), end);
        assert_eq!(logs.find(slot(2), Path::Live, mark(3, 0)), end);
        logs.hide(12289);
        let end = Found::End(mark(9, 0));
        assert_eq!(logs.find(slot(1), Path::Live, mark(8, 0)), end);
        assert_eq!(logs.find(slot(1), Path::Live, mark(9, 0)), end);
    }

    /// The durable end counts the hidden entries with no samples at its seq, and
    /// an entry synced later is found.
    #[test]
    fn a_read_past_hidden_entries_with_no_samples_finds_the_durable_end() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 4, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 4, 0, None), 8192)
            .expect("syncs");
        logs.hide(4096);
        assert_eq!(
            logs.find(slot(1), Path::Live, mark(4, 0)),
            Found::Run(channel::Key::from_u128(1), run(4, 0, 8192))
        );
        logs.hide(12288);
        let end = Found::End(mark(4, 1));
        assert_eq!(logs.find(slot(1), Path::Live, mark(4, 0)), end);
        assert_eq!(logs.find(slot(1), Path::Live, mark(3, 0)), end);
        logs.sync(slot(1), &header(1, Path::Live, 4, 0, None), 12288)
            .expect("syncs");
        assert_eq!(
            logs.find(slot(1), Path::Live, mark(4, 0)),
            Found::Run(channel::Key::from_u128(1), run(4, 1, 12288))
        );
    }

    /// No call of `Logs` shows the drop, which only bounds memory, so the test
    /// reads the runs.
    #[test]
    fn a_trim_drops_the_runs_that_it_hides() {
        let mut logs = three_records();
        assert_eq!(
            logs.kept(),
            Some(4096),
            "the second path holds its tail there"
        );
        logs.sync(slot(2), &header(2, Path::Live, 2, 1, None), 12288)
            .expect("syncs");
        assert_eq!(logs.kept(), Some(12288));
        logs.hide(8192);
        let left: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(left, [run(3, 0, 8192), run(6, 0, 12288)]);
        let other: Vec<Run> = logs.runs(slot(2), Path::Live).collect();
        assert_eq!(other, [run(2, 0, 12288)]);
        logs.hide(12288);
        let left: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(left, [run(6, 0, 12288)], "the run at the tail stays");
        assert_eq!(logs.kept(), Some(12288));
    }

    #[test]
    fn the_oldest_holder_of_a_tail_is_kept() {
        let mut logs = Logs::new(AREA);
        assert_eq!(logs.kept(), None, "no path has a synced entry");
        append(&mut logs, slot(1), &mut header(1, Path::Live, 0, 3, None))
            .expect("appends");
        assert_eq!(logs.kept(), None, "an appended entry holds nothing");
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 4096)
            .expect("syncs");
        logs.sync(slot(2), &header(2, Path::Live, 0, 1, None), 8192)
            .expect("syncs");
        assert_eq!(logs.kept(), Some(4096));
        logs.sync(slot(1), &header(1, Path::Live, 3, 1, None), 12288)
            .expect("syncs");
        assert_eq!(
            logs.kept(),
            Some(8192),
            "a later record holds the first tail"
        );
        logs.sync(slot(2), &header(2, Path::Live, 1, 1, None), 12288)
            .expect("syncs");
        assert_eq!(logs.kept(), Some(12288));
        logs.recover(slot(3), &carried_of(3, 5, 0), 16384)
            .expect("a new path takes any tail");
        assert_eq!(
            logs.kept(),
            Some(12288),
            "a carry record holds a later tail"
        );
    }

    /// S4 bounds the memory of the items by one entry per record. No call of `Logs`
    /// shows the entries, so the test reads them.
    #[test]
    fn the_items_of_a_record_share_its_entry() {
        let mut logs = Logs::new(AREA);
        for index in 1..=3 {
            logs.sync(slot(index), &header(index, Path::Live, 0, 1, None), 4096)
                .expect("syncs");
        }
        logs.recover(slot(4), &carried_of(4, 5, 0), 8192)
            .expect("a new path takes any tail");
        logs.recover(slot(5), &carried_of(5, 5, 0), 8192)
            .expect("a new path takes any tail");
        assert_eq!(logs.items.records, [(4096, 3), (8192, 5)]);
        assert_eq!(logs.items.places.len(), 5);
    }

    #[test]
    fn a_pop_gives_each_item_of_its_record() {
        let mut items = Items::default();
        items.push(4096, 1);
        items.push(4096, 2);
        items.push(8192, 3);
        let (offset, places) = items.pop(8192).expect("a record before the tail");
        assert_eq!((offset, places.collect::<Vec<_>>()), (4096, vec![1, 2]));
        assert!(items.pop(8192).is_none(), "only the record at 8192 is left");
    }

    #[test]
    #[should_panic(expected = "invariant: record 4096 comes after record 8192")]
    fn a_record_before_a_hidden_holder_is_a_broken_invariant() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 8192)
            .expect("syncs");
        logs.hide(12288);
        logs.sync(slot(1), &header(1, Path::Live, 3, 1, None), 4096)
            .expect("the invariant panics first");
    }

    /// The tail of index 1 on the live path that a carry record holds.
    fn carried(seq: u64, given: u64, stamp: Option<i64>) -> carry::Tail {
        carry::Tail {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            seq,
            given,
            stamp: stamp.map(Stamp::from_nanos),
        }
    }

    /// The tail of `index` on the live path, with no stamp, that a carry record
    /// holds.
    fn carried_of(index: u32, seq: u64, given: u64) -> carry::Tail {
        carry::Tail {
            index: channel::Key::from_u128(u128::from(index)),
            ..carried(seq, given, None)
        }
    }

    /// Logs that the walk fed one record at 4096 with the entry [0, 3) of index 1
    /// on the live path, with `last`, and an entry with no samples at 3.
    fn walked(last: Option<i64>) -> Logs {
        let mut logs = Logs::new(AREA);
        for mut header in [
            header(1, Path::Live, 0, 3, last),
            header(1, Path::Live, 3, 0, None),
        ] {
            append(&mut logs, slot(1), &mut header).expect("appends");
            logs.sync(slot(1), &header, 4096).expect("syncs");
        }
        logs
    }

    mod carry_tail {
        use super::*;

        #[test]
        fn moves_a_new_path_to_the_carried_tail_with_no_run() {
            let mut logs = Logs::new(AREA);
            logs.recover(slot(1), &carried(7, 2, Some(70)), 8192)
                .expect("a new path takes any tail");
            assert_eq!(logs.kept(), Some(8192), "the carry record holds the tail");
            let tail = Tail {
                seq: 7,
                stamp: Some(Stamp::from_nanos(70)),
            };
            assert_eq!(logs.appended(slot(1), Path::Live), tail);
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            assert_eq!(logs.runs(slot(1), Path::Live).count(), 0);
            let end = Found::End(mark(7, 2));
            assert_eq!(logs.find(slot(1), Path::Live, mark(0, 0)), end);
            assert_eq!(logs.find(slot(1), Path::Live, mark(7, 2)), end);
            assert_eq!(logs.durable(slot(1), Path::Backfill), Tail::default());
        }

        #[test]
        fn keeps_the_tail_and_the_runs_of_a_path_at_its_end() {
            let mut logs = walked(Some(30));
            let tail = logs.durable(slot(1), Path::Live);
            logs.recover(slot(1), &carried(3, 1, Some(30)), 8192)
                .expect("the carried tail is the path's tail");
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            assert_eq!(logs.appended(slot(1), Path::Live), tail);
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(0, 0, 4096)]);
            assert_eq!(logs.kept(), Some(8192), "the carry record holds the tail");
            let key = channel::Key::from_u128(1);
            let found = Found::Run(key, run(0, 0, 4096));
            assert_eq!(logs.find(slot(1), Path::Live, mark(0, 0)), found);
            let end = Found::End(mark(3, 1));
            assert_eq!(logs.find(slot(1), Path::Live, mark(3, 1)), end);
        }

        #[test]
        fn gives_a_path_with_no_stamp_the_carried_stamp() {
            let mut logs = walked(None);
            logs.recover(slot(1), &carried(3, 1, Some(20)), 8192)
                .expect("a trim passed the stamp");
            let tail = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(20)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            assert_eq!(logs.appended(slot(1), Path::Live), tail);
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(0, 0, 4096)]);
        }

        /// A trim passed two of four entries with no samples at seq 3, and the
        /// walk met the other two, which count 3 and 4.
        #[test]
        fn starts_a_new_path_past_the_entries_that_a_trim_passed() {
            let mut logs = Logs::new(AREA);
            for (given, offset) in [(3, 4096), (4, 8192)] {
                let header = Header {
                    kind: Kind::Record(NonZeroU64::new(given).expect("from 1")),
                    ..header(1, Path::Live, 3, 0, None)
                };
                logs.walk(slot(1), &header, offset).expect("walks");
            }
            logs.recover(slot(1), &carried(3, 4, Some(20)), 12288)
                .expect("the walk met the end");
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(3, 2, 4096), run(3, 3, 8192)]);
            let key = channel::Key::from_u128(1);
            assert_eq!(
                logs.find(slot(1), Path::Live, mark(3, 0)),
                Found::Run(key, run(3, 2, 4096))
            );
            assert_eq!(
                logs.find(slot(1), Path::Live, mark(3, 3)),
                Found::Run(key, run(3, 3, 8192))
            );
            assert_eq!(
                logs.find(slot(1), Path::Live, mark(3, 4)),
                Found::End(mark(3, 4))
            );
            let tail = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(20)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            assert_eq!(logs.appended(slot(1), Path::Live), tail);
            let mut tails = Carried::default();
            logs.clone().carry(16384, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(3, 4, Some(20))]);
            assert_eq!(logs.kept(), Some(12288));
        }

        /// The entry at 8192 is in a record that the commit took, so the tail goes
        /// past it.
        #[test]
        fn carry_gives_the_appended_tail_of_a_taken_record() {
            let mut logs = walked(Some(20));
            append(&mut logs, slot(1), &mut header(1, Path::Live, 3, 2, None))
                .expect("appends");
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(5, 0, Some(20))]);
        }

        #[test]
        fn hold_makes_the_carry_record_the_holder() {
            let mut logs = walked(Some(20));
            let mut header = header(1, Path::Live, 3, 2, None);
            append(&mut logs, slot(1), &mut header).expect("appends");
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            logs.sync(slot(1), &header, 8192).expect("syncs");
            assert_eq!(logs.kept(), Some(4096));
            logs.hold(&tails, 12288);
            assert_eq!(logs.kept(), Some(12288));
            let tail = Tail {
                seq: 5,
                stamp: Some(Stamp::from_nanos(20)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(0, 0, 4096), run(3, 1, 8192)]);
            logs.carry(16384, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(5, 0, Some(20))]);
            assert_eq!(logs.kept(), Some(12288));
        }

        /// A commit with no blocks for its carry record drops it, and the next scan
        /// gives the tail again. A scan that is not dropped goes on past it, and a
        /// drop of no tails keeps the scan where it is.
        #[test]
        fn carry_after_a_dropped_carry_gives_its_tails_again() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(3, 1, Some(20))]);
            logs.dropped(&mut tails);
            assert_eq!(tails.tails(), []);
            logs.carry(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(3, 1, Some(20))]);
            logs.carry(8192, 2, &mut tails);
            assert_eq!(tails.tails(), []);
            logs.dropped(&mut tails);
            logs.carry(8192, 2, &mut tails);
            assert_eq!(tails.tails(), []);
        }

        /// The entry of seq 5 came after the commit took its records.
        #[test]
        fn hold_keeps_the_appended_tail_of_a_queued_path() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            append(
                &mut logs,
                slot(1),
                &mut header(1, Path::Live, 3, 2, Some(50)),
            )
            .expect("appends");
            logs.hold(&tails, 8192);
            let durable = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(20)),
            };
            let appended = Tail {
                seq: 5,
                stamp: Some(Stamp::from_nanos(50)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), durable);
            assert_eq!(logs.appended(slot(1), Path::Live), appended);
            assert_eq!(logs.kept(), Some(8192));
        }

        #[test]
        #[should_panic(expected = "invariant: the carried tail")]
        fn a_carried_tail_that_is_not_the_durable_end_is_a_broken_invariant() {
            let mut logs = walked(Some(20));
            append(&mut logs, slot(1), &mut header(1, Path::Live, 3, 2, None))
                .expect("appends");
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            logs.hold(&tails, 12288);
        }

        #[test]
        #[should_panic(expected = "invariant: carry record 4096 comes after record")]
        fn a_carry_record_at_the_holder_is_a_broken_invariant() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.carry(8192, 2, &mut tails);
            logs.hold(&tails, 4096);
        }

        /// A trim that passes the carry record of a path keeps the run of a later
        /// record.
        #[test]
        fn a_trim_past_a_carry_record_keeps_the_later_run() {
            let mut logs = walked(None);
            logs.recover(slot(1), &carried(3, 1, None), 8192)
                .expect("the walk met the end");
            let mut header = header(1, Path::Live, 3, 2, None);
            append(&mut logs, slot(1), &mut header).expect("appends");
            logs.sync(slot(1), &header, 12288).expect("syncs");
            logs.hide(12288);
            let later = run(3, 1, 12288);
            assert_eq!(
                logs.find(slot(1), Path::Live, mark(0, 0)),
                Found::Run(channel::Key::from_u128(1), later)
            );
            assert_eq!(logs.kept(), Some(12288));
        }

        /// A carried tail at seq 3 came first. A later one that counts more entries
        /// with no samples than the walk met is a wrong shape.
        #[test]
        fn refuses_more_entries_with_no_samples_after_a_carried_tail_came_first() {
            let mut logs = Logs::new(AREA);
            logs.recover(slot(1), &carried(3, 1, None), 4096)
                .expect("a new path");
            let mut header = header(1, Path::Live, 3, 0, None);
            append(&mut logs, slot(1), &mut header).expect("appends");
            logs.sync(slot(1), &header, 8192).expect("syncs");
            let before = logs.clone();
            let tail = carried(3, 3, None);
            let invalid = logs.recover(slot(1), &tail, 12288);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 2),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
            assert_eq!(logs, before);
            let mut logs = Logs::new(AREA);
            logs.recover(slot(1), &carried(3, 1, None), 4096)
                .expect("a new path");
            let tail = carried(3, 2, None);
            let invalid = logs.recover(slot(1), &tail, 8192);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 1),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
        }

        /// The carried tail at 8192 counts on from the walk. A later one that counts
        /// more entries with no samples than the walk met is a wrong shape.
        #[test]
        fn refuses_more_entries_with_no_samples_after_a_carried_tail_of_a_met_path() {
            let mut logs = Logs::new(AREA);
            let mut header = header(1, Path::Live, 3, 0, None);
            append(&mut logs, slot(1), &mut header).expect("appends");
            logs.sync(slot(1), &header, 4096).expect("syncs");
            logs.recover(slot(1), &carried(3, 1, None), 8192)
                .expect("the walk met the end");
            append(&mut logs, slot(1), &mut header).expect("appends");
            logs.sync(slot(1), &header, 12288).expect("syncs");
            let before = logs.clone();
            let tail = carried(3, 3, None);
            let invalid = logs.recover(slot(1), &tail, 16384);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 2),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
            assert_eq!(logs, before);
        }

        /// The record at 8192 gave the path its tail, as a new path and as a met
        /// one.
        #[test]
        fn refuses_a_carry_record_that_lists_a_path_twice() {
            let mut new = Logs::new(AREA);
            let tail = carried(5, 0, None);
            new.recover(slot(1), &tail, 8192).expect("a new path");
            let mut met = walked(None);
            let end = carried(3, 1, None);
            met.recover(slot(1), &end, 8192)
                .expect("the walk met the end");
            let repeated = Invalid::Repeated {
                index: channel::Key::from_u128(1),
                path: Path::Live,
            };
            for (logs, tail) in [(new, tail), (met, end)] {
                let mut again = logs.clone();
                assert_eq!(again.recover(slot(1), &tail, 8192), Err(repeated));
                assert_eq!(again, logs);
            }
            assert_eq!(
                repeated.to_string(),
                "a carry record lists index 00000000-0000-0000-0000-000000000001 on \
                 path Live twice"
            );
        }

        /// The walk met an entry with no samples at seq 3, then one at seq 5.
        #[test]
        fn refuses_more_entries_with_no_samples_when_a_walked_one_is_at_a_lower_seq() {
            let mut logs = Logs::new(AREA);
            for (first, offset) in [(3, 4096), (5, 8192)] {
                let mut header = header(1, Path::Live, first, 0, None);
                append(&mut logs, slot(1), &mut header).expect("appends");
                logs.sync(slot(1), &header, offset).expect("syncs");
            }
            let before = logs.clone();
            let tail = carried(5, 2, None);
            let invalid = logs.recover(slot(1), &tail, 12288);
            let carried = Invalid::Carried {
                carried: tail,
                end: mark(5, 1),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried));
            assert_eq!(logs, before);
            logs.recover(slot(1), &carried_of(1, 5, 1), 12288)
                .expect("the carried end is the walked end");
        }

        #[test]
        fn refuses_a_carried_tail_that_is_not_the_path_tail_and_changes_nothing() {
            let refused = [
                (Some(30), carried(2, 5, Some(30))),
                (Some(30), carried(3, 0, Some(30))),
                (Some(30), carried(3, 2, Some(30))),
                (Some(30), carried(4, 0, Some(30))),
                (Some(30), carried(3, 1, None)),
                (Some(30), carried(3, 1, Some(31))),
                (None, carried(4, 0, Some(30))),
            ];
            for (last, tail) in refused {
                let logs = walked(last);
                let mut carried_to = logs.clone();
                let invalid = carried_to
                    .recover(slot(1), &tail, 8192)
                    .expect_err("the carried tail is not the path's");
                let carried = Invalid::Carried {
                    carried: tail,
                    end: mark(3, 1),
                    stamp: last.map(Stamp::from_nanos),
                };
                assert_eq!(invalid, carried, "carried {tail:?}");
                assert_eq!(carried_to, logs, "carried {tail:?}");
            }
            let invalid = walked(Some(30))
                .recover(slot(1), &carried(4, 0, None), 8192)
                .expect_err("the carried end is ahead");
            assert_eq!(
                invalid.to_string(),
                "the carried tail of index 00000000-0000-0000-0000-000000000001 on \
                 path Live at seq 4 after 0 empty entries with stamp None is not the \
                 end at seq 3 after 1 with stamp Some(Stamp(30))"
            );
        }

        #[test]
        #[should_panic(expected = "invariant: slot 1 holds index \
                                   00000000-0000-0000-0000-000000000001 and index \
                                   00000000-0000-0000-0000-000000000002")]
        fn a_slot_with_a_second_index_is_a_broken_invariant() {
            let mut logs = walked(Some(30));
            let other = carry::Tail {
                index: channel::Key::from_u128(2),
                ..carried(3, 1, Some(30))
            };
            logs.recover(slot(1), &other, 8192)
                .expect("the invariant panics first");
        }

        /// A carry of a path with entries that are not durable keeps them.
        #[test]
        fn a_carry_of_a_queued_path_keeps_its_appended_tail() {
            let mut logs = walked(Some(30));
            let mut queued = header(1, Path::Live, 3, 2, Some(50));
            append(&mut logs, slot(1), &mut queued).expect("appends");
            logs.recover(slot(1), &carried(3, 1, Some(30)), 8192)
                .expect("the walk met the end");
            let appended = Tail {
                seq: 5,
                stamp: Some(Stamp::from_nanos(50)),
            };
            assert_eq!(logs.appended(slot(1), Path::Live), appended);
            assert_eq!(logs.kept(), Some(8192));
            logs.sync(slot(1), &queued, 12288).expect("syncs");
            assert_eq!(logs.durable(slot(1), Path::Live), appended);
        }
    }

    #[test]
    #[should_panic(expected = "invariant: the hidden tail 4096 goes back from 8192")]
    fn a_hidden_tail_that_goes_back_is_a_broken_invariant() {
        let mut logs = three_records();
        logs.hide(8192);
        logs.hide(8192);
        logs.hide(4096);
    }

    /// Gives each entry of `records` the count that an append stores.
    fn counted(records: &mut [(u64, Header)]) {
        let mut logs = Logs::new(AREA);
        for (_, header) in records {
            append(&mut logs, slot(1), header).expect("appends");
        }
    }

    /// Feeds `records` to logs as the walk does, from the record at `from` on.
    fn walk_from(records: &[(u64, Header)], from: u64) -> Logs {
        let mut walked = Logs::new(AREA);
        for (offset, header) in records.iter().filter(|(offset, _)| *offset >= from) {
            walked.walk(slot(1), header, *offset).expect("walks");
        }
        walked
    }

    /// Trims `records` to the oldest holder, which leaves no tail to carry, and
    /// asserts that a walk of the records left gives the durable position.
    fn trim_to_the_holder(mut records: Vec<(u64, Header)>, holder: u64) {
        counted(&mut records);
        let records = records.as_slice();
        let mut logs = walk_from(records, 0);
        assert_eq!(logs.kept(), Some(holder));
        let mut tails = Carried::default();
        logs.carry(holder, 1023, &mut tails);
        assert_eq!(tails.tails(), []);
        logs.hide(holder);
        let walked = walk_from(records, holder);
        let durable =
            |logs: &Logs| logs.log(slot(1), Path::Live).map(|log| log.durable);
        assert_eq!(durable(&walked), durable(&logs));
    }

    /// An entry with no samples has no stamp, so the record of the stamp holds.
    #[test]
    fn a_trim_to_the_oldest_holder_keeps_the_stamp() {
        let records = [
            (4096, header(1, Path::Live, 0, 3, Some(30))),
            (8192, header(1, Path::Live, 3, 0, None)),
        ];
        trim_to_the_holder(records.to_vec(), 4096);
    }

    /// Each entry with no samples stores its count, so the newest one holds.
    #[test]
    fn a_trim_to_the_oldest_holder_keeps_the_count_of_entries_with_no_samples() {
        let records = [
            (4096, header(1, Path::Live, 3, 0, None)),
            (8192, header(1, Path::Live, 3, 0, None)),
        ];
        trim_to_the_holder(records.to_vec(), 8192);
    }

    /// An entry that alone gives the durable position moves the holder.
    #[test]
    fn an_entry_that_gives_the_position_alone_moves_the_holder() {
        let records = [
            (4096, header(1, Path::Live, 0, 3, Some(30))),
            (8192, header(1, Path::Live, 3, 0, None)),
            (12288, header(1, Path::Live, 3, 2, Some(50))),
        ];
        trim_to_the_holder(records.to_vec(), 12288);
    }

    /// Records at 4096 and 8192 each hold an entry with no samples at seq 0, and the
    /// one at 12288 holds [0, 1). A walk from any record counts each entry as the
    /// append did, so a read past the first entry gets the second.
    #[test]
    fn a_walk_from_any_record_gives_the_marks_of_the_append() {
        let mut records = [
            (4096, header(1, Path::Live, 0, 0, None)),
            (8192, header(1, Path::Live, 0, 0, None)),
            (12288, header(1, Path::Live, 0, 1, None)),
        ];
        counted(&mut records);
        let key = channel::Key::from_u128(1);
        let reads = [mark(0, 0), mark(0, 1), mark(0, 2)];
        let found = [
            (4096, [run(0, 0, 4096), run(0, 1, 8192), run(0, 2, 12288)]),
            (8192, [run(0, 1, 8192), run(0, 1, 8192), run(0, 2, 12288)]),
            (12288, [run(0, 0, 12288); 3]),
        ];
        for (from, runs) in found {
            let walked = walk_from(&records, from);
            for (before, run) in reads.into_iter().zip(runs) {
                assert_eq!(
                    walked.find(slot(1), Path::Live, before),
                    Found::Run(key, run),
                    "a walk from {from}, a read at {before:?}"
                );
            }
            let end = mark(1, 0);
            assert_eq!(walked.find(slot(1), Path::Live, end), Found::End(end));
        }
    }

    /// The walk met two entries with no samples at seq 3, so a third that counts 2
    /// or 4 is a wrong shape, and the logs do not change.
    #[test]
    fn a_walk_refuses_a_count_that_is_not_the_walked_one() {
        let mut records = [
            (4096, header(1, Path::Live, 3, 0, None)),
            (8192, header(1, Path::Live, 3, 0, None)),
        ];
        counted(&mut records);
        let logs = walk_from(&records, 0);
        for given in [2, 4] {
            let entry = Header {
                kind: Kind::Record(NonZeroU64::new(given).expect("from 1")),
                ..header(1, Path::Live, 3, 0, None)
            };
            let mut walked = logs.clone();
            let counted = Invalid::Counted {
                index: channel::Key::from_u128(1),
                path: Path::Live,
                seq: 3,
                given,
                walked: 3,
            };
            assert_eq!(walked.walk(slot(1), &entry, 12288), Err(counted));
            assert_eq!(walked, logs);
        }
        let counted = Invalid::Counted {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            seq: 3,
            given: 4,
            walked: 3,
        };
        assert_eq!(
            counted.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live with \
             no samples at seq 3 counts 4, and the walk counts 3"
        );
    }

    /// The walk takes the largest count, and an append or a walk of one more entry
    /// with no samples at its seq is crowded, and the logs do not change.
    #[test]
    fn an_entry_with_no_samples_after_the_largest_count_is_crowded() {
        let largest = Header {
            kind: Kind::Record(NonZeroU64::MAX),
            ..header(1, Path::Live, 3, 0, None)
        };
        let logs = walk_from(&[(4096, largest)], 0);
        let crowded = Invalid::Crowded {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            seq: 3,
        };
        let mut appended = logs.clone();
        let mut next = header(1, Path::Live, 3, 0, None);
        assert_eq!(append(&mut appended, slot(1), &mut next), Err(crowded));
        assert_eq!(appended, logs);
        let mut walked = logs.clone();
        assert_eq!(walked.walk(slot(1), &largest, 8192), Err(crowded));
        assert_eq!(walked, logs);
        assert_eq!(
            crowded.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live with \
             no samples follows u64::MAX of them at seq 3"
        );
    }

    #[test]
    fn after_the_largest_count_other_entries_append() {
        let largest = Header {
            kind: Kind::Record(NonZeroU64::MAX),
            ..header(1, Path::Live, 3, 0, None)
        };
        let logs = walk_from(&[(4096, largest)], 0);
        let mut later = logs.clone();
        let mut skip = header(1, Path::Live, 5, 0, None);
        assert_eq!(append(&mut later, slot(1), &mut skip), Ok(()));
        assert_eq!(skip.kind, Kind::Record(NonZeroU64::MIN));
        let mut samples = logs;
        let mut data = header(1, Path::Live, 3, 2, Some(9));
        assert_eq!(append(&mut samples, slot(1), &mut data), Ok(()));
    }

    /// The bytes that the runs and the items of `logs` hold. No call of `Logs` shows
    /// the room of its deques, so the tests of S4 read it.
    fn held(logs: &Logs) -> (usize, usize) {
        let runs: usize = logs
            .paths
            .iter()
            .map(|log| log.runs.capacity() * size_of::<Run>())
            .sum();
        let items = logs.items.places.capacity() * size_of::<u32>()
            + logs.items.records.capacity() * size_of::<(u64, u64)>();
        (runs, items)
    }

    /// The bound of S4 on the bytes that [`held`] counts in a ring of `records`
    /// one-block records with `paths` paths: "at most 488/51 of the area, plus 32 bytes
    /// per record", and the room of the runs of each path with no run, 96 bytes.
    fn s4_bound(records: u64, paths: u64) -> u64 {
        4096 * records * 488 / 51 + 32 * records + 96 * paths
    }

    /// 65 one-block data records each hold an entry with no samples at seq 0 for each
    /// of 80 paths: 80 * 51 + 13 bytes fit in 4096.
    #[test]
    fn runs_of_entries_with_no_samples_keep_memory_in_the_s4_bound() {
        let records: u64 = 65;
        let paths: u32 = 80;
        let mut logs = Logs::new(4096 * records);
        for record in 0..records {
            let offset = 4096 * (record + 1);
            for index in 1..=paths {
                let mut entry = header(index, Path::Live, 0, 0, None);
                append(&mut logs, slot(index), &mut entry).expect("appends");
                logs.sync(slot(index), &entry, offset).expect("syncs");
            }
        }
        let (runs, items) = held(&logs);
        let held = u64::try_from(runs + items).expect("fits");
        let bound = s4_bound(records, u64::from(paths));
        assert!(
            held <= bound,
            "runs {runs} + items {items} = {held} bytes, over the bound of {bound} for \
             {records} records"
        );
    }

    /// The tests of #2636, with the bound of S4.
    mod breaker {
        use super::*;

        /// One one-block record with one entry of each of 80 paths.
        #[test]
        fn one_record_of_80_paths_stays_in_the_s4_bound() {
            let paths: u32 = 80;
            let mut logs = Logs::new(4096);
            for index in 1..=paths {
                let mut entry = header(index, Path::Live, 0, 1, Some(1));
                append(&mut logs, slot(index), &mut entry).expect("appends");
                logs.sync(slot(index), &entry, 4096).expect("syncs");
            }
            let (runs, items) = held(&logs);
            let total = u64::try_from(runs + items).expect("fits");
            let bound = s4_bound(1, u64::from(paths));
            assert!(
                total <= bound,
                "runs {runs} + items {items} = {total} bytes, over \
                 the bound of {bound} for one record"
            );
        }

        /// A ring of 65 records. Group after group of 80 paths fills its 64 data
        /// records; each group then goes idle, a carry record takes the tails of each
        /// group, and a trim to that carry record frees the records before it. So a
        /// path gets a trim each lap after its group, and at most three of them shrink
        /// its room to two runs.
        #[test]
        fn trimmed_records_keep_memory_in_the_s4_bound() {
            let paths: u32 = 80;
            let per_group: u64 = 64;
            let groups: u32 = 8;
            let ring_records = per_group + 1;
            let mut logs = Logs::new(4096 * ring_records);
            let mut offset = 0u64;
            for group in 0..groups {
                for _ in 0..per_group {
                    offset += 4096;
                    for index in 1..=paths {
                        let index = group * paths + index;
                        let at = slot(index);
                        let first = logs.appended(at, Path::Live).seq;
                        let mut entry = header(index, Path::Live, first, 1, Some(1));
                        append(&mut logs, at, &mut entry).expect("appends");
                        logs.sync(at, &entry, offset).expect("syncs");
                    }
                }
                offset += 4096;
                let mut tails = Carried::default();
                logs.carry(offset, usize::MAX, &mut tails);
                logs.hold(&tails, offset);
                assert_eq!(logs.kept(), Some(offset));
                logs.hide(offset);
            }
            let rooms: Vec<usize> = (0..groups)
                .map(|group| {
                    let at = slot(group * paths + 1);
                    let log = logs.log(at, Path::Live).expect("the path has a log");
                    log.runs.capacity()
                })
                .collect();
            assert_eq!(rooms, [2, 2, 2, 2, 2, 64, 64, 64]);
            let (runs, items) = held(&logs);
            let total = u64::try_from(runs + items).expect("fits");
            let bound = s4_bound(ring_records, u64::from(groups * paths));
            assert!(
                total <= bound,
                "runs {runs} + items {items} = {total} bytes, over \
                 the bound of {bound} for a ring of {ring_records} records"
            );
        }
    }

    /// Logs of a ring of `area` bytes, with one path that has a run in each of the
    /// records 1 to `count`, of a block each.
    fn one_path_with_runs(area: u64, count: u64) -> Logs {
        let mut logs = Logs::new(area);
        for record in 1..=count {
            let mut entry = header(1, Path::Live, record - 1, 1, Some(1));
            append(&mut logs, slot(1), &mut entry).expect("appends");
            logs.sync(slot(1), &entry, 4096 * record).expect("syncs");
        }
        logs
    }

    /// The room and the count of the runs of the path of index 1, which no call of
    /// `Logs` shows. The `memory` test checks the room through the bytes of the heap.
    fn room(logs: &Logs) -> (usize, usize) {
        let log = logs.log(slot(1), Path::Live).expect("the path has a log");
        (log.runs.capacity(), log.runs.len())
    }

    /// A log with room for `room` runs and a run in each of the records 1 to `runs`,
    /// of a block each, in a lap that starts at 0.
    fn log_with(room: usize, runs: u64) -> Log {
        let mut log = Log::new(channel::Key::from_u128(1), Path::Live);
        log.runs = VecDeque::with_capacity(room);
        log.runs
            .extend((1..=runs).map(|record| run(record - 1, 0, 4096 * record)));
        log
    }

    fn room_of(log: &Log) -> (usize, usize) {
        (log.runs.capacity(), log.runs.len())
    }

    /// A budget of `most` shrinks, with no trim.
    fn budget(most: usize) -> Budget {
        Budget {
            most,
            trims: 0,
            used: 0,
        }
    }

    /// Trims `log`, at place 0, at `offset`, and ends its lap when the trim does.
    fn trim(laps: &mut Laps, budget: &mut Budget, log: &mut Log, offset: u64) {
        if log.trim(offset, AREA) {
            laps.end(budget, log, 0, offset);
        }
    }

    #[test]
    fn a_trim_inside_a_lap_keeps_the_room() {
        let mut log = log_with(16, 3);
        trim(&mut Laps::new(), &mut budget(1 << 20), &mut log, 4096);
        assert_eq!(room_of(&log), (16, 2));
        assert_eq!((log.lap, log.peak), (0, 3));
    }

    #[test]
    fn the_trim_that_ends_a_lap_keeps_the_room_of_runs_that_fill_a_quarter_of_it() {
        let mut log = log_with(16, 4);
        trim(&mut Laps::new(), &mut budget(1 << 20), &mut log, AREA);
        assert_eq!(room_of(&log), (16, 4));
        assert_eq!((log.lap, log.peak), (AREA, 4));
    }

    /// The lap met 5 runs, though 1 is left at its end.
    #[test]
    fn the_trim_that_ends_a_lap_shrinks_the_room_to_twice_the_most_runs_of_the_lap() {
        let (mut laps, mut budget) = (Laps::new(), budget(1 << 20));
        let mut log = log_with(32, 5);
        for record in 1..=4 {
            trim(&mut laps, &mut budget, &mut log, 4096 * record);
        }
        assert_eq!(room_of(&log), (32, 1));
        trim(&mut laps, &mut budget, &mut log, AREA);
        assert_eq!(room_of(&log), (10, 1));
        assert_eq!((log.lap, log.peak), (AREA, 1));
    }

    #[test]
    fn the_trim_that_ends_a_lap_with_no_run_keeps_room_for_two() {
        let (mut laps, mut budget) = (Laps::new(), budget(1 << 20));
        let mut log = log_with(8, 1);
        trim(&mut laps, &mut budget, &mut log, 4096);
        trim(&mut laps, &mut budget, &mut log, AREA);
        assert_eq!(room_of(&log), (2, 0));
    }

    #[test]
    fn a_lap_ends_at_the_first_trim_an_area_after_its_start() {
        let (mut laps, mut budget) = (Laps::new(), budget(1 << 20));
        let mut log = log_with(16, 0);
        log.lap = 4096;
        trim(&mut laps, &mut budget, &mut log, AREA);
        assert_eq!((room_of(&log), log.lap), ((16, 0), 4096));
        trim(&mut laps, &mut budget, &mut log, AREA + 4096);
        assert_eq!((room_of(&log), log.lap), ((2, 0), AREA + 4096));
    }

    /// The lap waits at 3 runs met, then meets 5. The end of a hide with no shrink
    /// keeps it in the queue once, and the next ends it with a shrink to twice 5.
    #[test]
    fn a_lap_end_with_no_shrink_left_waits_and_counts_the_runs_of_the_whole_lap() {
        let mut laps = Laps::new();
        let mut log = log_with(32, 3);
        trim(&mut laps, &mut budget(0), &mut log, AREA);
        assert_eq!((room_of(&log), log.lap, log.due), ((32, 3), 0, true));
        assert_eq!(laps.due, [0]);
        log.runs
            .extend([run(3, 0, AREA + 4096), run(4, 0, AREA + 8192)]);
        assert!(!log.trim(AREA + 4096, AREA));
        assert_eq!((room_of(&log), log.lap, log.peak), ((32, 5), 0, 5));
        laps.finish(&mut budget(0), std::slice::from_mut(&mut log), 2 * AREA);
        assert_eq!((room_of(&log), log.lap, log.due), ((32, 5), 0, true));
        assert_eq!(laps.due, [0]);
        let mut next = budget(1);
        laps.finish(&mut next, std::slice::from_mut(&mut log), 2 * AREA);
        assert_eq!((room_of(&log), next.used, laps.due.len()), ((10, 5), 1, 0));
        assert_eq!((log.lap, log.peak, log.due), (2 * AREA, 5, false));
    }

    #[test]
    fn a_lap_end_with_no_shrink_left_and_none_needed_does_not_wait() {
        let mut laps = Laps::new();
        let mut log = log_with(16, 4);
        trim(&mut laps, &mut budget(0), &mut log, AREA);
        assert_eq!((room_of(&log), log.lap, log.due), ((16, 4), AREA, false));
        assert!(laps.due.is_empty());
    }

    /// A path syncs 14 runs while its lap end waits, so the drain keeps room for 16.
    #[test]
    fn a_drain_keeps_room_for_the_runs_synced_in_the_wait() {
        let mut logs = Logs::new(4 * 4096);
        logs.laps.most = 0;
        let mut seq = 0;
        let mut sync = |logs: &mut Logs, offset: u64| {
            let mut entry = header(1, Path::Live, seq, 1, Some(1));
            append(logs, slot(1), &mut entry).expect("appends");
            logs.sync(slot(1), &entry, offset).expect("syncs");
            seq += 1;
        };
        for record in 1..=13 {
            sync(&mut logs, 4096 * record);
            if record > 8 {
                logs.hide(4096 * record);
            }
        }
        assert!(logs.paths[0].due, "the lap end waits");
        assert_eq!(room(&logs), (16, 1));
        for n in 1..=14 {
            sync(&mut logs, 4096 * 13 + 100 * n);
        }
        assert_eq!(room(&logs), (16, 15));
        logs.laps.most = 1;
        logs.hide(logs.hidden);
        assert!(!logs.paths[0].due, "the drain ends the lap");
        sync(&mut logs, 4096 * 13 + 100 * 15);
        assert_eq!(room(&logs), (16, 16));
    }

    #[test]
    fn a_shrink_at_a_lap_end_takes_one_from_the_count() {
        let mut budget = budget(2);
        let mut log = log_with(32, 3);
        trim(&mut Laps::new(), &mut budget, &mut log, AREA);
        assert_eq!((room_of(&log), budget.used), ((6, 3), 1));
    }

    #[test]
    fn each_four_trims_of_a_hide_add_a_shrink() {
        let (mut laps, mut budget) = (Laps::new(), budget(0));
        budget.trimmed(3);
        let mut log = log_with(32, 3);
        trim(&mut laps, &mut budget, &mut log, AREA);
        assert_eq!((room_of(&log), log.due), ((32, 3), true));
        laps.due.clear();
        budget.trimmed(1);
        let mut log = log_with(32, 3);
        trim(&mut laps, &mut budget, &mut log, AREA);
        assert_eq!((room_of(&log), log.due, budget.used), ((6, 3), false, 1));
    }

    /// Each of 100,000 paths has an entry in each record of a ring of 4 blocks. The
    /// first 8 records stay, so each path has room for 16 runs. Then each commit
    /// trims to its own record, and the lap that holds 2 runs needs a shrink of each
    /// path at the trim of record 12. That `hide`, and each `hide` after it, trims
    /// 100,000 runs, so it shrinks 256 + 25,000 paths, first in, first out, until
    /// each has room for 4.
    #[test]
    fn a_hide_shrinks_256_paths_and_one_for_each_four_trims_and_the_rest_wait_in_turn()
    {
        let paths: u32 = 100_000;
        let mut logs = Logs::new(4 * 4096);
        let rooms = |logs: &Logs| -> Vec<usize> {
            logs.paths.iter().map(|log| log.runs.capacity()).collect()
        };
        let mut shrunk = Vec::new();
        for record in 1..=17 {
            for index in 1..=paths {
                let at = slot(index);
                let first = logs.appended(at, Path::Live).seq;
                let mut entry = header(index, Path::Live, first, 1, Some(1));
                append(&mut logs, at, &mut entry).expect("appends");
                logs.sync(at, &entry, 4096 * record).expect("syncs");
            }
            if record > 8 {
                let before = rooms(&logs);
                logs.hide(4096 * record);
                let after = rooms(&logs);
                let places: Vec<usize> = (0..before.len())
                    .filter(|&at| after[at] < before[at])
                    .collect();
                let range = places.first().zip(places.last()).map(|(&a, &b)| a..b + 1);
                assert_eq!(places.len(), range.clone().map_or(0, |r| r.len()));
                shrunk.push(range);
            }
            if record == 14 {
                assert!(
                    logs.laps.due.capacity() <= 2 * 100_000,
                    "S4 counts 8 bytes a path"
                );
            }
        }
        let budget = 256 + 100_000 / 4;
        let mut expected = vec![None; 4];
        expected.extend([
            Some(0..budget),
            Some(budget..2 * budget),
            Some(2 * budget..3 * budget),
            Some(3 * budget..100_000),
            None,
        ]);
        assert_eq!(shrunk, expected);
        assert!(logs.laps.due.is_empty());
        assert!(logs.paths.iter().all(|log| log.runs.capacity() == 4));
    }

    /// Writes one record at `offset`, with an entry of 51 bytes of each of
    /// `indexes`, and moves `offset` past it.
    fn record_at(
        logs: &mut Logs,
        offset: &mut u64,
        records: &mut Vec<u64>,
        indexes: impl IntoIterator<Item = u32>,
    ) {
        let at = *offset;
        let mut count = 0;
        for index in indexes {
            let s = slot(index);
            let first = logs.appended(s, Path::Live).seq;
            let mut entry = header(index, Path::Live, first, 1, Some(1));
            append(logs, s, &mut entry).expect("appends");
            logs.sync(s, &entry, at).expect("syncs");
            count += 1;
        }
        *offset += 51 * count;
        records.push(at);
    }

    /// 8192 paths, each with an entry in each sweep record, and `most` shrinks in a
    /// hide with no trim. 9 sweeps stay, then each commit trims to the oldest holder,
    /// so one hide needs a shrink of each path, and most wait. In each of the next
    /// `bursts` commits, one path late in the queue gets 65,535 records of its own,
    /// which fit in the ring. Returns the most bytes of room of the runs after a
    /// hide, and the S4 bound of the runs and items (1256/51 of the area plus 32
    /// bytes per record) plus 96 bytes per path.
    fn bursts_in_a_wait(most: usize, bursts: u32) -> (usize, usize) {
        let paths: u32 = 8192;
        let area: u64 = 512 * u64::from(paths);
        let mut logs = Logs::new(area);
        logs.laps.most = most;
        let mut offset = 0;
        let mut records = Vec::new();
        for _ in 0..9 {
            record_at(&mut logs, &mut offset, &mut records, 1..=paths);
        }
        let mut queued = false;
        let mut burst = 0;
        let mut worst = (0, 0);
        for _ in 0..400 {
            record_at(&mut logs, &mut offset, &mut records, 1..=paths);
            assert!(offset - logs.hidden <= area, "the ring holds the records");
            let kept = logs.kept().expect("a holder");
            logs.hide(kept);
            records.retain(|&record| record >= logs.hidden);
            queued |= logs.paths[0].runs.capacity() < 16;
            if queued && burst < bursts {
                let path = paths - burst;
                for _ in 0..65_535 {
                    record_at(&mut logs, &mut offset, &mut records, [path]);
                }
                burst += 1;
                assert!(offset - logs.hidden <= area, "the ring holds the records");
            }
            let room = logs
                .paths
                .iter()
                .map(|log| log.runs.capacity())
                .sum::<usize>()
                * size_of::<Run>();
            let bound = usize::try_from(1256 * area / 51).expect("fits")
                + 32 * records.len()
                + 96 * usize::try_from(paths).expect("fits");
            if room > worst.0 {
                worst = (room, bound);
            }
            if burst == bursts && logs.laps.due.is_empty() {
                break;
            }
        }
        worst
    }

    /// With 16 shrinks in a hide and no shrink for its trims, a lap end waits 512
    /// hides, and the room passes the bound.
    #[test]
    fn the_runs_of_paths_that_wait_keep_the_s4_bound() {
        let (room, bound) = bursts_in_a_wait(16, 60);
        assert!(room <= bound, "{room} bytes of room, over {bound}");
    }

    /// The lap and the peak fit in the padding of a log, and a bucket of the map of
    /// places holds 12 bytes. S4 counts both.
    #[test]
    fn a_log_takes_144_bytes_and_its_place_12() {
        assert_eq!(size_of::<Log>(), 144);
        assert_eq!(size_of::<((Slot, Path), u32)>(), 12);
    }

    /// Appends and syncs the next entry of index `index` in the record at `offset`.
    fn synced(logs: &mut Logs, index: u32, offset: u64) {
        let first = logs
            .log(slot(index), Path::Live)
            .map_or(0, |log| log.appended.tail.seq + 1);
        let mut entry = header(index, Path::Live, first, 1, Some(10));
        append(logs, slot(index), &mut entry).expect("appends");
        logs.sync(slot(index), &entry, offset).expect("syncs");
    }

    /// Index 1 holds the record before the reach, index 2 syncs in each of the 12
    /// records after it, and index 3 holds the record after those. A call of limit 3
    /// takes the tail of index 1, then fills from the 12 items after the reach, so it
    /// takes index 2 and not index 3. The next call goes on at index 3.
    #[test]
    fn a_fill_walks_four_items_for_each_tail_after_the_reach() {
        let mut logs = Logs::new(AREA);
        synced(&mut logs, 1, 4096);
        for record in 2..=13 {
            synced(&mut logs, 2, 4096 * record);
        }
        synced(&mut logs, 3, 4096 * 14);
        let indexes = |tails: &Carried| -> Vec<u128> {
            tails
                .tails()
                .iter()
                .map(|tail| tail.index.as_u128())
                .collect()
        };
        let mut tails = Carried::default();
        logs.carry(8192, 3, &mut tails);
        assert_eq!(indexes(&tails), [1, 2]);
        logs.carry(4096 * 15, 3, &mut tails);
        assert_eq!(indexes(&tails), [3]);
    }

    /// A call clears `carried` first, so a call that takes no tail before the reach
    /// gives none and stops there, also when `carried` holds the tails of a call
    /// before it.
    #[test]
    fn a_carry_with_no_tail_before_the_reach_stops_at_it() {
        let mut logs = Logs::new(AREA);
        synced(&mut logs, 1, 4096);
        let mut tails = Carried::default();
        logs.carry(8192, 2, &mut tails);
        synced(&mut logs, 2, 12288);
        assert_eq!(tails.tails().len(), 1);
        logs.carry(8192, 2, &mut tails);
        assert_eq!(tails.tails(), []);
        logs.carry(16384, 2, &mut tails);
        let indexes: Vec<_> = tails.tails().iter().map(|tail| tail.index).collect();
        assert_eq!(indexes, [channel::Key::from_u128(2)]);
    }

    /// Gives the tail of each idle path a carry record at `offset`, as a commit does.
    fn carry_idle(logs: &mut Logs, offset: u64) {
        let mut tails = Carried::default();
        logs.carry(offset, usize::MAX, &mut tails);
        logs.hold(&tails, offset);
    }

    /// A path holds 8 runs in a ring of 8 blocks, then no run, and a carry record in
    /// each next block takes its tail, as a commit does. The trim of the record of
    /// the eighth run ends the lap that held 8 runs, and the trim 8 blocks later ends
    /// the lap that held none: from then on, the room is two.
    #[test]
    fn the_room_of_a_path_with_no_run_shrinks_at_the_end_of_the_next_lap() {
        let mut logs = one_path_with_runs(8 * 4096, 8);
        let mut rooms = Vec::new();
        for record in 9..=20 {
            carry_idle(&mut logs, 4096 * record);
            let kept = logs.kept().expect("the path has a holder");
            logs.hide(kept);
            rooms.push(room(&logs));
        }
        let mut expected = vec![(8, 0); 8];
        expected.extend([(2, 0); 4]);
        assert_eq!(rooms, expected);
    }

    /// In a ring of one block, each trim ends a lap.
    #[test]
    fn a_trim_keeps_room_for_two_runs_of_a_path_it_empties() {
        let mut logs = one_path_with_runs(4096, 8);
        carry_idle(&mut logs, 4096 * 9);
        logs.hide(4096 * 9);
        assert_eq!(room(&logs), (2, 0));
        let mut entry = header(1, Path::Live, 8, 1, Some(1));
        append(&mut logs, slot(1), &mut entry).expect("appends");
        logs.sync(slot(1), &entry, 4096 * 10).expect("syncs");
        assert_eq!(room(&logs), (2, 1));
        assert_eq!(
            logs.runs(slot(1), Path::Live).collect::<Vec<_>>(),
            [run(8, 0, 4096 * 10)]
        );
    }

    /// The runs that each path held after each step, the step before the trim that
    /// ended each of its laps, and the hides since its lap end began to wait.
    #[derive(Default)]
    struct Seen {
        held: Vec<usize>,
        ends: Vec<usize>,
        lap: u64,
        waits: usize,
    }

    proptest! {
        /// Records of a block each hold an entry of some of 8 paths, or the carried
        /// tails of the idle paths, and a trim may follow each, up to the oldest
        /// holder, in a ring of 4 blocks, with 1 or 2 shrinks for each hide and one
        /// for each 4 trims. Each lap spans at least the area. After each trim, each
        /// path has room for at most four times the most runs that it held from the
        /// step before the end of the lap before the current one, or four with no run.
        /// No hide shrinks more than its budget, and no lap end waits more hides than 8
        /// over the count, rounded up.
        #[test]
        fn a_trim_keeps_the_room_of_each_path_in_four_times_its_runs_of_two_laps(
            records in prop::collection::vec(
                (
                    prop::collection::vec(1..=8u32, 0..6),
                    any::<bool>(),
                    prop::option::of(0..80u64),
                ),
                1..60,
            ),
            shrinks in 1..=2usize,
        ) {
            let area = 4 * 4096;
            let mut logs = Logs::new(area);
            logs.laps.most = shrinks;
            let mut seen: Vec<Seen> = Vec::new();
            for (record, (indexes, carried, trim)) in (1..).zip(&records) {
                let offset = 4096 * record;
                if *carried {
                    carry_idle(&mut logs, offset);
                } else {
                    for &index in indexes {
                        let at = slot(index);
                        let first = logs.appended(at, Path::Live).seq;
                        let mut entry = header(index, Path::Live, first, 1, Some(1));
                        append(&mut logs, at, &mut entry).expect("appends");
                        prop_assert_eq!(logs.sync(at, &entry, offset), Ok(()));
                    }
                }
                seen.resize_with(logs.paths.len(), Seen::default);
                for (log, laps) in logs.paths.iter().zip(&mut seen) {
                    laps.held.push(log.runs.len());
                }
                let Some(trim) = trim else { continue };
                let Some(kept) = logs.kept() else { continue };
                let rooms: Vec<usize> =
                    logs.paths.iter().map(|log| log.runs.capacity()).collect();
                let tail = logs.hidden.max((4096 * trim).min(kept));
                let trims = logs
                    .items
                    .since(0)
                    .filter(|&(_, offset, _)| offset < tail)
                    .count();
                logs.hide(tail);
                let shrunk = logs
                    .paths
                    .iter()
                    .zip(&rooms)
                    .filter(|&(log, &room)| log.runs.capacity() < room)
                    .count();
                prop_assert!(
                    shrunk <= shrinks + trims / 4,
                    "{} shrinks in one hide of {} trims",
                    shrunk,
                    trims
                );
                for (log, laps) in logs.paths.iter().zip(&mut seen) {
                    let ended = log.lap != laps.lap;
                    if ended {
                        prop_assert!(log.lap >= laps.lap + area);
                        laps.ends.push(laps.held.len() - 1);
                        laps.lap = log.lap;
                    }
                    laps.waits = match (log.due, ended) {
                        (false, _) => 0,
                        (true, true) => 1,
                        (true, false) => laps.waits + 1,
                    };
                    prop_assert!(laps.waits <= 8_usize.div_ceil(shrinks));
                    laps.held.push(log.runs.len());
                    let from = laps.ends.len().checked_sub(2).map_or(0, |n| laps.ends[n]);
                    let most = laps.held[from..].iter().copied().max().unwrap_or(0);
                    let room = log.runs.capacity();
                    prop_assert!(
                        room <= 4 * most.max(1),
                        "room {} for at most {} runs of two laps",
                        room,
                        most
                    );
                }
            }
        }
    }

    /// A record of a model ring, at its offset: the tails a commit carried, or the
    /// entries it synced.
    #[derive(Clone, Debug)]
    enum Written {
        Carry(u64, Vec<carry::Tail>),
        Data(u64, Vec<(u32, Header)>),
    }

    /// A `Next` with many entries with no samples at few seqs.
    fn short_next() -> impl Strategy<Value = Next> {
        (
            0..3u32,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..2u64,
            prop_oneof![Just(0u32), Just(0u32), Just(1u32), Just(2u32)],
            prop::option::of(0..3i64),
        )
            .prop_map(|(index, path, skip, len, last)| Next {
                index,
                path,
                skip,
                len,
                last,
            })
    }

    proptest! {
        /// Commits that carry the idle tails after their data record and trim at or
        /// below `kept`, then a walk of the records left, as the open does, gives
        /// each path its durable position and each read what it gets.
        #[test]
        fn a_walk_after_each_trim_gives_what_the_logs_give(
            records in prop::collection::vec(
                (
                    prop::collection::vec(short_next(), 0..4),
                    0..14u64,
                    0..4usize,
                    prop::option::of(0..14u64),
                    any::<bool>(),
                    prop::bool::weighted(0.2),
                ),
                1..16,
            ),
        ) {
            let mut logs = Logs::new(AREA);
            let mut ring = Vec::new();
            let mut hidden = 0;
            let numbered = records.iter().zip(1..);
            for ((record, reach, limit, trim, carries, reopen), number) in numbered {
                if *reopen {
                    let walked = walk_ring(&ring, hidden)?;
                    same_reads(&ring, &walked, &logs)?;
                    logs = walked;
                }
                let synced_at = 8192 * number;
                let carried_at = synced_at + 4096;
                let mut queued = Vec::new();
                for next in record {
                    let at = slot(next.index);
                    let first = logs.appended(at, next.path).seq + next.skip;
                    let mut header =
                        header(next.index, next.path, first, next.len, next.last);
                    append(&mut logs, at, &mut header).expect("appends");
                    queued.push((next.index, header));
                }
                let mut carried = Carried::default();
                logs.carry(8192 * *reach.min(&number), *limit, &mut carried);
                for (index, header) in &queued {
                    prop_assert_eq!(logs.sync(slot(*index), header, synced_at), Ok(()));
                }
                if !queued.is_empty() {
                    ring.push(Written::Data(synced_at, queued));
                }
                if *carries && !carried.tails().is_empty() {
                    logs.hold(&carried, carried_at);
                    ring.push(Written::Carry(carried_at, carried.tails().to_vec()));
                }
                if let Some(trim) = trim {
                    let bound = logs.kept().unwrap_or(synced_at + 4096);
                    let target = (8192 * trim).min(bound);
                    hidden = hidden.max(target);
                    logs.hide(hidden);
                }
            }
            let walked = walk_ring(&ring, hidden)?;
            same_reads(&ring, &walked, &logs)?;
        }
    }

    /// The logs that a walk of the records of `ring` at or after `hidden` builds.
    fn walk_ring(ring: &[Written], hidden: u64) -> Result<Logs, TestCaseError> {
        let mut walked = Logs::new(AREA);
        for record in ring {
            match record {
                Written::Carry(offset, tails) if *offset >= hidden => {
                    for tail in tails {
                        let at = slot(index_of(tail.index));
                        prop_assert_eq!(walked.recover(at, tail, *offset), Ok(()));
                    }
                }
                Written::Data(offset, entries) if *offset >= hidden => {
                    for (index, header) in entries {
                        let at = slot(*index);
                        prop_assert_eq!(walked.walk(at, header, *offset), Ok(()));
                    }
                }
                _ => {}
            }
        }
        Ok(walked)
    }

    /// Checks that each path of `walked` has the durable position of `logs`, and
    /// that a read from each mark of a small range gets from `walked` what it gets
    /// from `logs`.
    fn same_reads(
        ring: &[Written],
        walked: &Logs,
        logs: &Logs,
    ) -> Result<(), TestCaseError> {
        for index in 0..3 {
            for path in [Path::Live, Path::Backfill] {
                let durable =
                    |logs: &Logs| logs.log(slot(index), path).map(|log| log.durable);
                prop_assert_eq!(durable(walked), durable(logs));
                for seq in 0..14 {
                    for given in 0..5 {
                        let from = mark(seq, given);
                        let found =
                            |logs: &Logs| match logs.find(slot(index), path, from) {
                                Found::Run(key, run) => {
                                    Ok((run.offset, read(ring, key, path, run, from)))
                                }
                                Found::End(end) => Err(end),
                            };
                        prop_assert_eq!(
                            found(walked),
                            found(logs),
                            "index {} path {:?} from {:?}",
                            index,
                            path,
                            from
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Each entry of `key` on `path` in the data record of `run` in `ring` that a
    /// read from `from` gets, with the mark after it, as the read's scan counts.
    fn read(
        ring: &[Written],
        key: channel::Key,
        path: Path,
        run: Run,
        from: Mark,
    ) -> Vec<(Header, Mark)> {
        let entries = ring
            .iter()
            .find_map(|record| match record {
                Written::Data(offset, entries) if *offset == run.offset => {
                    Some(entries)
                }
                _ => None,
            })
            .expect("a run names a data record");
        let mut at = run.start;
        entries
            .iter()
            .filter(|(_, header)| (header.index, header.path) == (key, path))
            .filter_map(|&(_, header)| {
                at = at.after(header.first, header.len());
                (at > from).then_some((header, at))
            })
            .collect()
    }
}
