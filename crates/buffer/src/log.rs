//! Where each path of each index stands: with every appended entry, on disk, and
//! in which records.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::collections::VecDeque;
use std::collections::vec_deque::Drain;
use std::fmt;
use std::num::NonZeroU8;

use types::channel::{self, Slot};
use types::frame::Path;
use types::hash;
use types::time::Stamp;

use crate::carry;
use crate::entry::Header;

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
        }
    }
}

impl Tail {
    /// Moves the tail past the entry: `seq` to `first + len`, and `stamp` to
    /// `last` when the entry has one. A `first` past the tail is a skip ahead.
    ///
    /// # Errors
    ///
    /// [`Invalid`] when `first` is below the tail or `first + len` does not fit in
    /// a `u64`. The tail does not move.
    pub(crate) fn advance(&mut self, header: &Header) -> Result<(), Invalid> {
        if header.first < self.seq {
            return Err(Invalid::Below {
                index: header.index,
                path: header.path,
                first: header.first,
                tail: self.seq,
            });
        }
        let past = Invalid::Past {
            index: header.index,
            path: header.path,
            first: header.first,
            len: header.len,
        };
        self.seq = header
            .first
            .checked_add(u64::from(header.len))
            .ok_or(past)?;
        if let Some(last) = header.last {
            self.stamp = Some(last);
        }
        Ok(())
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
    /// `given` passes `u64::MAX`: it starts at most at `carry::GIVEN_MAX`, and a path
    /// gets far fewer entries at one seq.
    pub(crate) fn after(self, first: u64, len: u32) -> Self {
        if len != 0 {
            let seq = first
                .checked_add(u64::from(len))
                .expect("invariant: the tail checked the entry");
            return Self::at(seq);
        }
        let given = if first == self.seq { self.given } else { 0 };
        Self {
            seq: first,
            given: given
                .checked_add(1)
                .expect("invariant: a ring holds fewer entries"),
        }
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

    /// Moves past the entry, as [`Tail::advance`], and returns the mark before it.
    fn advance(&mut self, header: &Header) -> Result<Mark, Invalid> {
        let start = self.mark();
        self.tail.advance(header)?;
        self.given = start.after(header.first, header.len).given;
        Ok(start)
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
    /// The offset of the newest record of the path that alone gives its durable
    /// position, stamp included: a data record whose entry of the path, advanced
    /// from the start, gives it, or a carry record of the path. A walk from it
    /// rebuilds that position. `None` before the first synced entry.
    holder: Option<u64>,
    /// The offset at which the lap of the path started: 0, or the offset of the trim
    /// that ended the lap before.
    lap: u64,
    /// The most runs that a trim of the lap met.
    peak: u32,
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
        }
    }

    /// Whether an appended entry is not durable. Marks only rise, so equal
    /// positions hold the same entries.
    fn queued(&self) -> bool {
        self.appended != self.durable
    }

    /// Drops the oldest run when it is of the record at `offset`. The first trim at
    /// least `area` after the start of the lap ends it: when the room is over
    /// four times the most runs that a trim of the lap met, or over four with none, it
    /// shrinks the room to twice them, or to two. So runs that swing inside a lap take
    /// no allocation, and the room is at most four times the most runs of two laps.
    fn trim(&mut self, offset: u64, area: u64) {
        let met = u32::try_from(self.runs.len()).unwrap_or(u32::MAX);
        if self.runs.front().is_some_and(|run| run.offset == offset) {
            self.runs.pop_front();
        }
        let peak = self.peak.max(met);
        if offset < self.lap.saturating_add(area) {
            self.peak = peak;
            return;
        }
        self.lap = offset;
        self.peak = 0;
        // No deque holds more than `isize::MAX` bytes, so no shift overflows.
        let keep = usize::try_from(peak)
            .expect("invariant: u32 fits usize")
            .max(1);
        if self.runs.capacity() > keep << 2 {
            std::hint::cold_path();
            self.runs.shrink_to(keep << 1);
        }
    }

    /// Moves the durable tail past the entry, and makes the record at `offset` the
    /// holder when the entry alone gives the durable position, stamp included.
    /// When the record has no run of the path, adds one.
    ///
    /// # Panics
    ///
    /// When `offset` is below the newest record of the path.
    fn sync(&mut self, header: &Header, offset: u64) -> Result<Synced, Invalid> {
        let start = self.durable.advance(header)?;
        let newest = self.runs.back().map(|run| run.offset).max(self.holder);
        if let Some(newest) = newest {
            assert!(
                newest <= offset,
                "invariant: record {offset} comes after record {newest}"
            );
        }
        if self.holder == Some(offset) {
            return Ok(Synced::Same);
        }
        let alone = Mark::at(0).after(header.first, header.len) == self.durable.mark();
        if alone && header.last == self.durable.tail.stamp {
            self.holder = Some(offset);
        }
        if self.runs.back().is_some_and(|run| run.offset == offset) {
            return Ok(Synced::Same);
        }
        self.runs.push_back(Run { start, offset });
        Ok(match newest {
            Some(after) if !alone => Synced::Marked { after },
            _ => Synced::Run,
        })
    }
}

/// What [`Log::sync`] did to the runs of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Synced {
    /// The record already had a run of the path.
    Same,
    /// It added a run.
    Run,
    /// It added a run whose first entry has no samples and comes after other
    /// entries with no samples at its seq. A walk from after the record at `after`
    /// up to and with this one does not know their count.
    Marked { after: u64 },
}

/// Every path the buffer holds, by the node's slot of the index and the path.
/// `append` feeds it with each appended entry; the recovery walk and each sync
/// feed it with each record's entries, in ring order. A trim hides the records
/// before its tail.
#[derive(Clone, Debug)]
pub(crate) struct Logs {
    /// The bytes of the ring: the least span of a lap of a path.
    area: u64,
    /// The place of the log of each path in `paths`.
    places: hash::Map<(Slot, Path), u32>,
    /// The log of each path, in the order that each first came. No log leaves, so a
    /// place in `places` and `items` stays valid. A change that drops a log updates
    /// both.
    paths: Vec<Log>,
    /// The offset of the newest record that holds a durable entry with each nonzero
    /// tag, by the log and the tag. Tag 0 marks samples, which no caller looks up. A
    /// change that drops a log drops its tags in the same place.
    tags: hash::Map<(Slot, Path, NonZeroU8), u64>,
    /// No read finds a record before this offset.
    hidden: u64,
    /// One item for each run and each carried tail. An item is stale once its path
    /// has a later holder.
    items: Items,
    /// Each item before this number is stale.
    stale: u64,
    /// An [`idle`](Self::idle) has passed each item before this number.
    scanned: u64,
    /// The ranges in which no trim lands. Each holds the range of each marked run,
    /// from the previous record of its path to the run's record.
    marked: Marked,
}

/// Compares what the logs hold. `stale` and `scanned` only skip work, so the syncs
/// whose trims moved them and a walk of the same records may differ in them.
#[cfg(test)]
impl PartialEq for Logs {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            area,
            places,
            paths,
            tags,
            hidden,
            items,
            stale: _,
            scanned: _,
            marked,
        } = self;
        *area == other.area
            && *places == other.places
            && *paths == other.paths
            && *tags == other.tags
            && *hidden == other.hidden
            && *items == other.items
            && *marked == other.marked
    }
}

#[cfg(test)]
impl Eq for Logs {}

impl Logs {
    /// The logs of a ring of `area` bytes, with no path.
    pub(crate) fn new(area: u64) -> Self {
        Self {
            area,
            places: hash::Map::default(),
            paths: Vec::new(),
            tags: hash::Map::default(),
            hidden: 0,
            items: Items::default(),
            stale: 0,
            scanned: 0,
            marked: Marked::default(),
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

    /// The durable end of `path` of the index at `slot`, as a carry record holds
    /// it.
    ///
    /// # Panics
    ///
    /// When the path has no log.
    #[cfg(feature = "sim")]
    pub(crate) fn end(&self, slot: Slot, path: Path) -> carry::Tail {
        let log = self.log(slot, path).expect("a log of the path");
        carry::Tail {
            index: log.index,
            path,
            seq: log.durable.tail.seq,
            given: log.durable.given,
            stamp: log.durable.tail.stamp,
        }
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

    /// The offset of the newest record that holds a durable entry with `tag` on
    /// `path`, for each index whose record is not hidden, with its slot and index,
    /// in no order.
    pub(crate) fn tagged(
        &self,
        path: Path,
        tag: NonZeroU8,
    ) -> Vec<(u64, Slot, channel::Key)> {
        let tagged = self.tags.iter().filter(|&(&(_, on, of), &offset)| {
            (on, of) == (path, tag) && offset >= self.hidden
        });
        tagged
            .map(|(&(slot, ..), &offset)| {
                let log = self.log(slot, path);
                (
                    offset,
                    slot,
                    log.expect("invariant: a tag has its log").index,
                )
            })
            .collect()
    }

    /// Hides the records before the offset `tail` from each later
    /// [`find`](Self::find) and [`tagged`](Self::tagged), and drops their runs. It
    /// visits each path once for each run that it drops.
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
        self.hidden = tail;
        self.marked.hide(tail);
        while let Some((offset, places)) = self.items.pop(tail) {
            for at in places {
                nth_mut(&mut self.paths, at).trim(offset, self.area);
            }
        }
    }

    /// The ranges in which no trim lands. A trim never lands after the previous
    /// record of a path up to and with a record whose first entry of the path has no
    /// samples and comes after other entries with no samples at its seq: the walk
    /// would count them from 0. [`Marked::land`] gives the newest offset at or before
    /// an offset that is in no such range.
    pub(crate) fn marked(&self) -> &Marked {
        &self.marked
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

    /// Puts in `carried` the appended tail of each path whose holder is before
    /// `reach`, at most `limit`, oldest holder first. The next call goes on at the
    /// first path that this call left out. Call it in the borrow that takes the
    /// records of a commit: each appended entry is then in a taken record, so the
    /// tail is the end after those records, and the carry record goes right after
    /// them.
    pub(crate) fn idle(&mut self, reach: u64, limit: usize, carried: &mut Carried) {
        let mut room = limit;
        for (number, offset, at) in self.items.since(self.scanned) {
            if offset >= reach {
                self.scanned = number;
                return;
            }
            let log = nth(&self.paths, at);
            if log.holder != Some(offset) {
                continue;
            }
            let Some(left) = room.checked_sub(1) else {
                self.scanned = number;
                return;
            };
            if carried.tails.is_empty() {
                carried.first = number;
            }
            room = left;
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

    /// Takes back the [`idle`](Self::idle) call that filled `carried`, whose carry
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
    pub(crate) fn carried(&mut self, carried: &Carried, offset: u64) {
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
    pub(crate) fn carry(
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
        let met = carried.seq == end.seq
            && carried.given == end.given
            && stamp.is_none_or(|_| carried.stamp == stamp);
        if !met {
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

    /// Moves the appended tail of the header's path past the entry, as
    /// [`Tail::advance`].
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Tail::advance`]. Nothing changes.
    pub(crate) fn append(
        &mut self,
        slot: Slot,
        header: &Header,
    ) -> Result<(), Invalid> {
        self.change(slot, header, |log| log.appended.advance(header))
            .map(|_| ())
    }

    /// Moves the durable tail of the header's path past the entry, as
    /// [`Tail::advance`], and adds the record at `offset` to the path's runs when it
    /// is not the newest. The record becomes the path's holder when the entry alone
    /// gives the durable position, stamp included. A new run whose first entry has
    /// no samples and comes after other entries with no samples at its seq marks the
    /// range from after the previous record of the path up to and with `offset`, in
    /// which [`Marked::land`] lands no trim. A nonzero tag makes `offset` the
    /// newest record of its tag on the path.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Tail::advance`]. Nothing changes.
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
        let (synced, at) = self.change(slot, header, |log| log.sync(header, offset))?;
        if synced != Synced::Same {
            self.items.push(offset, at);
        }
        if let Synced::Marked { after } = synced {
            self.marked.mark(after, offset);
        }
        if let Some(tag) = NonZeroU8::new(header.tag) {
            self.tags.insert((slot, header.path, tag), offset);
        }
        Ok(())
    }

    /// Applies `change` to the log of the header's path, which starts empty when
    /// the path is new, and gives the place of the log. A change that fails on a new
    /// path adds nothing.
    ///
    /// # Panics
    ///
    /// When the log of `slot` on the header's path holds another index than the
    /// header's.
    fn change<T>(
        &mut self,
        slot: Slot,
        header: &Header,
        change: impl FnOnce(&mut Log) -> Result<T, Invalid>,
    ) -> Result<(T, u32), Invalid> {
        let key = (slot, header.path);
        if let Some(&at) = self.places.get(&key) {
            let log = nth_mut(&mut self.paths, at);
            assert!(
                log.index == header.index,
                "invariant: slot {} holds index {} and index {}",
                slot.get(),
                log.index,
                header.index,
            );
            return Ok((change(log)?, at));
        }
        let mut log = Log::new(header.index, header.path);
        let changed = change(&mut log)?;
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

/// The ranges of records in which no trim lands: each from after the record `after`
/// up to and with a later record. They are sorted, with a gap between any two.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct Marked(VecDeque<(u64, u64)>);

impl Marked {
    /// Adds the range from after `after` up to and with `record`, which is at or
    /// after the newest record of each range. Merges each range that it touches or
    /// covers.
    fn mark(&mut self, after: u64, record: u64) {
        let covered = |&(before, last): &(u64, u64)| last == record && before <= after;
        if self.0.back().is_some_and(covered) {
            return;
        }
        let mut first = after;
        while let Some(&(before, last)) = self.0.back() {
            if last < first {
                break;
            }
            first = first.min(before);
            self.0.pop_back();
        }
        self.0.push_back((first, record));
    }

    /// The start of the range that holds `offset`, else `offset`. One binary search.
    pub(crate) fn land(&self, offset: u64) -> u64 {
        let at = self.0.partition_point(|&(_, record)| record < offset);
        self.0
            .get(at)
            .map_or(offset, |&(after, _)| after.min(offset))
    }

    /// Drops each range whose newest record is before `tail`.
    fn hide(&mut self, tail: u64) {
        while self.0.front().is_some_and(|&(_, record)| record < tail) {
            self.0.pop_front();
        }
    }
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
            len,
            stored_at: Stamp::from_nanos(7),
            last: last.map(Stamp::from_nanos),
            tag: 0,
            bytes: 0,
        }
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
                .map(|header| header.first + u64::from(header.len))
                .max()
                .unwrap_or(0),
            stamp: on(fed, slot, path).filter_map(|header| header.last).last(),
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
            at = at.after(header.first, header.len);
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
                    let header = header(next.index, next.path, first, next.len, next.last);
                    prop_assert_eq!(logs.append(at, &header), Ok(()));
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
                        end = before.after(header.first, header.len);
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
                            before.after(header.first, header.len)
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
                    let header =
                        header(next.index, next.path, first, next.len, next.last);
                    prop_assert_eq!(logs.append(at, &header), Ok(()));
                    queued.push((next.index, header));
                }
                let reach = 8192 * *reach.min(&number);
                let mut carried = Carried::default();
                logs.idle(reach, *limit, &mut carried);
                let holders = model.holders();
                for (index, header) in &queued {
                    prop_assert_eq!(logs.sync(slot(*index), header, synced_at), Ok(()));
                    model.sync(*index, header, synced_at);
                }
                model.check(carried.tails(), reach, *limit, &holders)?;
                if *carries {
                    logs.carried(&carried, carried_at);
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
    /// durable end, and the stamp. Also the holders whose tail `idle` gave.
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

        /// Checks the tails of one call of `idle` up to `reach` with `limit`, made
        /// when each path had its holder in `holders`, after the sync of the
        /// entries that it saw appended.
        fn check(
            &mut self,
            tails: &[carry::Tail],
            reach: u64,
            limit: usize,
            holders: &hash::Map<(u32, Path), u64>,
        ) -> Result<(), TestCaseError> {
            prop_assert!(tails.len() <= limit);
            let mut previous = 0;
            for tail in tails {
                let key = (index_of(tail.index), tail.path);
                let holder = holders[&key];
                let (_, end, stamp) = self.paths[&key];
                prop_assert!(
                    self.hidden <= holder && holder < reach,
                    "holder {}",
                    holder
                );
                prop_assert!(previous <= holder, "oldest holder first");
                previous = holder;
                prop_assert_eq!(
                    (tail.seq, tail.given, tail.stamp),
                    (end.seq, end.given, stamp)
                );
                let first = self.given.insert((key.0, key.1, holder));
                prop_assert!(first, "given twice");
            }
            if tails.len() < limit {
                let left = holders.iter().find(|&(&(index, path), &holder)| {
                    holder < reach && !self.given.contains(&(index, path, holder))
                });
                prop_assert!(left.is_none(), "{:?} is left out", left);
            }
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
            let end = end.after(header.first, header.len);
            let stamp = header.last.or(stamp);
            let alone = (Mark::at(0).after(header.first, header.len), header.last);
            let holder = if alone == (end, stamp) {
                offset
            } else {
                holder
            };
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
        logs.append(slot(1), &header(1, Path::Live, 0, 3, Some(30)))
            .expect("appends");
        logs.append(slot(1), &header(1, Path::Backfill, 0, 1, Some(10)))
            .expect("appends");
        logs.append(slot(2), &header(2, Path::Live, 0, 5, None))
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
        logs.append(slot(1), &header(1, Path::Live, 0, 2, Some(2)))
            .expect("appends");
        logs.append(slot(1), &header(1, Path::Live, 10, 1, Some(11)))
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
        logs.append(slot(1), &header(1, Path::Live, 0, 2, Some(2)))
            .expect("appends");
        logs.append(slot(1), &header(1, Path::Live, 2, 0, None))
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
        logs.append(slot(1), &header(1, Path::Live, 0, 3, None))
            .expect("appends");
        let invalid = Invalid::Below {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: 2,
            tail: 3,
        };
        assert_eq!(
            logs.append(slot(1), &header(1, Path::Live, 2, 1, Some(9))),
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
            logs.append(slot(1), &header(1, Path::Live, u64::MAX, 1, Some(9))),
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
        logs.append(slot(1), &header(1, Path::Live, 0, 3, None))
            .expect("appends");
        logs.append(slot(1), &header(2, Path::Live, 3, 2, None))
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
        logs.append(slot(1), &header(1, Path::Live, 0, 3, None))
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
        logs.carry(slot(3), &carried_of(3, 5, 0), 16384)
            .expect("a new path takes any tail");
        assert_eq!(
            logs.kept(),
            Some(12288),
            "a carry record holds a later tail"
        );
    }

    /// A later entry of a path in the record that holds it keeps the run and the
    /// holder, and still gives its tag that record.
    #[test]
    fn a_later_entry_of_a_held_record_is_the_same_run() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 4096)
            .expect("syncs");
        let tagged = Header {
            tag: 2,
            ..header(1, Path::Live, 3, 1, None)
        };
        logs.sync(slot(1), &tagged, 4096).expect("syncs");
        let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(runs, [run(0, 0, 4096)]);
        assert_eq!(
            logs.durable(slot(1), Path::Live),
            Tail {
                seq: 4,
                stamp: None
            }
        );
        assert_eq!(logs.kept(), Some(4096));
        let tag = NonZeroU8::new(2).expect("nonzero");
        let key = channel::Key::from_u128(1);
        assert_eq!(logs.tagged(Path::Live, tag), [(4096, slot(1), key)]);
        let mut carried = Carried::default();
        logs.idle(8192, 2, &mut carried);
        assert_eq!(carried.tails().len(), 1, "one item in the record");
    }

    /// An entry with no samples whose mark alone is the durable one, with no stamp,
    /// does not hold a path that has a stamp.
    #[test]
    fn the_alone_compare_takes_the_stamp_of_the_entry() {
        let mut logs = Logs::new(AREA);
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, Some(30)), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 3, 0, None), 8192)
            .expect("syncs");
        assert_eq!(logs.kept(), Some(4096));
        let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(runs, [run(0, 0, 4096), run(3, 0, 8192)]);
        assert_eq!(logs.marked().land(8192), 8192, "no range is marked");
    }

    /// S4 bounds the memory of the items by one entry per record. The `alloc` test
    /// shows it only as a total: one `bench::Logs::commit` at 64 indexes counts 81
    /// allocations, and 85 when each item adds an entry. A pin of that total fails
    /// on each change of growth in another part, so the test reads the entries.
    /// `tests/memory` `per_path(4097)` also fails: 2007152 bytes for 4097 paths, over
    /// 1761710.
    #[test]
    fn the_items_of_a_record_share_its_entry() {
        let mut logs = Logs::new(AREA);
        for index in 1..=3 {
            logs.sync(slot(index), &header(index, Path::Live, 0, 1, None), 4096)
                .expect("syncs");
        }
        logs.carry(slot(4), &carried_of(4, 5, 0), 8192)
            .expect("a new path takes any tail");
        logs.carry(slot(5), &carried_of(5, 5, 0), 8192)
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
        for header in [
            header(1, Path::Live, 0, 3, last),
            header(1, Path::Live, 3, 0, None),
        ] {
            logs.append(slot(1), &header).expect("appends");
            logs.sync(slot(1), &header, 4096).expect("syncs");
        }
        logs
    }

    mod carry_tail {
        use super::*;

        #[test]
        fn moves_a_new_path_to_the_carried_tail_with_no_run() {
            let mut logs = Logs::new(AREA);
            logs.carry(slot(1), &carried(7, 2, Some(70)), 8192)
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
            logs.carry(slot(1), &carried(3, 1, Some(30)), 8192)
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
            logs.carry(slot(1), &carried(3, 1, Some(20)), 8192)
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

        /// The walk met two entries with no samples at seq 3. The landing rule keeps
        /// that count exact, so a carried end with four is a wrong shape.
        #[test]
        fn refuses_more_entries_with_no_samples_than_the_walk_met() {
            let mut logs = Logs::new(AREA);
            for offset in [4096, 8192] {
                let header = header(1, Path::Live, 3, 0, None);
                logs.append(slot(1), &header).expect("appends");
                logs.sync(slot(1), &header, offset).expect("syncs");
            }
            let before = logs.clone();
            let tail = carried(3, 4, Some(20));
            let invalid = logs.carry(slot(1), &tail, 12288);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 2),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
            assert_eq!(logs, before);
            logs.carry(slot(1), &carried(3, 2, Some(20)), 12288)
                .expect("the carried end is the walked end");
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(0, 0, 4096), run(3, 1, 8192)]);
            let tail = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(20)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            assert_eq!(logs.appended(slot(1), Path::Live), tail);
            assert_eq!(logs.kept(), Some(12288));
        }

        /// The entry at 8192 is in a record that the commit took, so the tail goes
        /// past it.
        #[test]
        fn idle_gives_the_appended_tail_of_a_taken_record() {
            let mut logs = walked(Some(20));
            logs.append(slot(1), &header(1, Path::Live, 3, 2, None))
                .expect("appends");
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(5, 0, Some(20))]);
        }

        #[test]
        fn carried_makes_the_carry_record_the_holder() {
            let mut logs = walked(Some(20));
            let header = header(1, Path::Live, 3, 2, None);
            logs.append(slot(1), &header).expect("appends");
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            logs.sync(slot(1), &header, 8192).expect("syncs");
            assert_eq!(logs.kept(), Some(4096));
            logs.carried(&tails, 12288);
            assert_eq!(logs.kept(), Some(12288));
            let tail = Tail {
                seq: 5,
                stamp: Some(Stamp::from_nanos(20)),
            };
            assert_eq!(logs.durable(slot(1), Path::Live), tail);
            let runs: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
            assert_eq!(runs, [run(0, 0, 4096), run(3, 1, 8192)]);
            tails.clear();
            logs.idle(16384, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(5, 0, Some(20))]);
            assert_eq!(logs.kept(), Some(12288));
        }

        /// A commit with no blocks for its carry record drops it, and the next scan
        /// gives the tail again. A scan that is not dropped goes on past it, and a
        /// drop of no tails keeps the scan where it is.
        #[test]
        fn idle_after_a_dropped_carry_gives_its_tails_again() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(3, 1, Some(20))]);
            logs.dropped(&mut tails);
            assert_eq!(tails.tails(), []);
            logs.idle(8192, 2, &mut tails);
            assert_eq!(tails.tails(), [carried(3, 1, Some(20))]);
            tails.clear();
            logs.idle(8192, 2, &mut tails);
            assert_eq!(tails.tails(), []);
            logs.dropped(&mut tails);
            logs.idle(8192, 2, &mut tails);
            assert_eq!(tails.tails(), []);
        }

        /// The entry of seq 5 came after the commit took its records.
        #[test]
        fn carried_keeps_the_appended_tail_of_a_queued_path() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            logs.append(slot(1), &header(1, Path::Live, 3, 2, Some(50)))
                .expect("appends");
            logs.carried(&tails, 8192);
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
            logs.append(slot(1), &header(1, Path::Live, 3, 2, None))
                .expect("appends");
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            logs.carried(&tails, 12288);
        }

        #[test]
        #[should_panic(expected = "invariant: carry record 4096 comes after record")]
        fn a_carry_record_at_the_holder_is_a_broken_invariant() {
            let mut logs = walked(Some(20));
            let mut tails = Carried::default();
            logs.idle(8192, 2, &mut tails);
            logs.carried(&tails, 4096);
        }

        /// A trim that passes the carry record of a path keeps the run of a later
        /// record.
        #[test]
        fn a_trim_past_a_carry_record_keeps_the_later_run() {
            let mut logs = walked(None);
            logs.carry(slot(1), &carried(3, 1, None), 8192)
                .expect("the walk met the end");
            let header = header(1, Path::Live, 3, 2, None);
            logs.append(slot(1), &header).expect("appends");
            logs.sync(slot(1), &header, 12288).expect("syncs");
            logs.hide(12288);
            let later = run(3, 1, 12288);
            assert_eq!(
                logs.find(slot(1), Path::Live, mark(0, 0)),
                Found::Run(channel::Key::from_u128(1), later)
            );
            assert_eq!(logs.kept(), Some(12288));
        }

        /// A carried tail at seq 3 came first: a later one with more entries with no
        /// samples than the walk met is a wrong shape.
        #[test]
        fn refuses_more_entries_with_no_samples_after_a_carried_tail_came_first() {
            let mut logs = Logs::new(AREA);
            logs.carry(slot(1), &carried(3, 1, None), 4096)
                .expect("a new path");
            let header = header(1, Path::Live, 3, 0, None);
            logs.append(slot(1), &header).expect("appends");
            logs.sync(slot(1), &header, 8192).expect("syncs");
            let before = logs.clone();
            let tail = carried(3, 3, None);
            let invalid = logs.carry(slot(1), &tail, 12288);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 2),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
            assert_eq!(logs, before);
            let mut logs = Logs::new(AREA);
            logs.carry(slot(1), &carried(3, 1, None), 4096)
                .expect("a new path");
            let tail = carried(3, 2, None);
            let invalid = logs.carry(slot(1), &tail, 8192);
            let carried_to = Invalid::Carried {
                carried: tail,
                end: mark(3, 1),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried_to));
        }

        /// The record at 8192 gave the path its tail, as a new path and as a met
        /// one.
        #[test]
        fn refuses_a_carry_record_that_lists_a_path_twice() {
            let mut new = Logs::new(AREA);
            let tail = carried(5, 0, None);
            new.carry(slot(1), &tail, 8192).expect("a new path");
            let mut met = walked(None);
            let end = carried(3, 1, None);
            met.carry(slot(1), &end, 8192)
                .expect("the walk met the end");
            let repeated = Invalid::Repeated {
                index: channel::Key::from_u128(1),
                path: Path::Live,
            };
            for (logs, tail) in [(new, tail), (met, end)] {
                let mut again = logs.clone();
                assert_eq!(again.carry(slot(1), &tail, 8192), Err(repeated));
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
                let header = header(1, Path::Live, first, 0, None);
                logs.append(slot(1), &header).expect("appends");
                logs.sync(slot(1), &header, offset).expect("syncs");
            }
            let before = logs.clone();
            let tail = carried(5, 2, None);
            let invalid = logs.carry(slot(1), &tail, 12288);
            let carried = Invalid::Carried {
                carried: tail,
                end: mark(5, 1),
                stamp: None,
            };
            assert_eq!(invalid, Err(carried));
            assert_eq!(logs, before);
            logs.carry(slot(1), &carried_of(1, 5, 1), 12288)
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
                    .carry(slot(1), &tail, 8192)
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
                .carry(slot(1), &carried(4, 0, None), 8192)
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
            logs.carry(slot(1), &other, 8192)
                .expect("the invariant panics first");
        }

        /// A carry of a path with entries that are not durable keeps them.
        #[test]
        fn a_carry_of_a_queued_path_keeps_its_appended_tail() {
            let mut logs = walked(Some(30));
            let queued = header(1, Path::Live, 3, 2, Some(50));
            logs.append(slot(1), &queued).expect("appends");
            logs.carry(slot(1), &carried(3, 1, Some(30)), 8192)
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

    /// Feeds `records` to logs as the walk does, from the record at `from` on.
    fn walk_from(records: &[(u64, Header)], from: u64) -> Logs {
        let mut walked = Logs::new(AREA);
        for (offset, header) in records.iter().filter(|(offset, _)| *offset >= from) {
            walked.append(slot(1), header).expect("appends");
            walked.sync(slot(1), header, *offset).expect("syncs");
        }
        walked
    }

    /// Trims `records` to the oldest holder, which leaves no tail to carry, and
    /// asserts that a walk of the records left gives the durable position.
    fn trim_to_the_holder(records: &[(u64, Header)], holder: u64) {
        let mut logs = walk_from(records, 0);
        assert_eq!(logs.kept(), Some(holder));
        let mut tails = Carried::default();
        logs.idle(holder, 1023, &mut tails);
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
        trim_to_the_holder(&records, 4096);
    }

    /// The record of the first entry with no samples at the seq holds the count.
    #[test]
    fn a_trim_to_the_oldest_holder_keeps_the_count_of_entries_with_no_samples() {
        let records = [
            (4096, header(1, Path::Live, 3, 0, None)),
            (8192, header(1, Path::Live, 3, 0, None)),
        ];
        trim_to_the_holder(&records, 4096);
    }

    /// An entry that alone gives the durable position moves the holder.
    #[test]
    fn an_entry_that_gives_the_position_alone_moves_the_holder() {
        let records = [
            (4096, header(1, Path::Live, 0, 3, Some(30))),
            (8192, header(1, Path::Live, 3, 0, None)),
            (12288, header(1, Path::Live, 3, 2, Some(50))),
        ];
        trim_to_the_holder(&records, 12288);
    }

    /// Records at 4096 and 8192 each hold an entry with no samples at seq 0, and the
    /// one at 12288 holds [0, 1). A walk from 8192 would count the entry there as the
    /// first at seq 0, so a landing there moves down to 4096, and a reader that got
    /// the first entry still gets the second.
    #[test]
    fn a_landing_after_entries_with_no_samples_at_a_seq_moves_before_them() {
        let records = [
            (4096, header(1, Path::Live, 0, 0, None)),
            (8192, header(1, Path::Live, 0, 0, None)),
            (12288, header(1, Path::Live, 0, 1, None)),
        ];
        let mut logs = walk_from(&records, 0);
        assert_eq!(logs.kept(), Some(12288));
        assert_eq!(logs.marked().land(12288), 12288);
        assert_eq!(logs.marked().land(8192), 4096);
        let walked = walk_from(&records, logs.marked().land(8192));
        assert_eq!(
            walked.find(slot(1), Path::Live, mark(0, 1)),
            Found::Run(channel::Key::from_u128(1), run(0, 1, 8192))
        );
    }

    /// A landing that moves down out of one range can land in a range of another
    /// path, and moves down again. A carry record ends a range.
    #[test]
    fn a_landing_moves_down_out_of_each_range_it_lands_in() {
        let mut logs = Logs::new(AREA);
        let empty = |index, seq| header(index, Path::Live, seq, 0, None);
        logs.sync(slot(2), &empty(2, 5), 4096).expect("syncs");
        logs.sync(slot(1), &empty(1, 0), 8192).expect("syncs");
        logs.sync(slot(2), &empty(2, 5), 12288).expect("syncs");
        logs.sync(slot(1), &empty(1, 0), 16384).expect("syncs");
        assert_eq!(logs.marked().land(16384), 4096);
        assert_eq!(logs.marked().land(20480), 20480);
        logs.carry(slot(1), &carried(0, 2, None), 20480)
            .expect("the walk met the end");
        logs.sync(slot(1), &empty(1, 0), 24576).expect("syncs");
        assert_eq!(logs.marked().land(24576), 20480);
        assert_eq!(logs.marked().land(8192), 4096);
    }

    /// A range that covers older ranges with a gap between them merges with each,
    /// so a landing in any of them moves down to its start in one step.
    #[test]
    fn a_landing_in_a_merged_range_moves_to_its_start() {
        let mut logs = Logs::new(AREA);
        let empty = |index, seq| header(index, Path::Live, seq, 0, None);
        logs.sync(slot(2), &empty(2, 5), 0).expect("syncs");
        logs.sync(slot(1), &empty(1, 0), 4096).expect("syncs");
        logs.sync(slot(1), &empty(1, 0), 8192).expect("syncs");
        logs.carry(slot(1), &carried(0, 2, None), 12288)
            .expect("the walk met the end");
        logs.sync(slot(1), &empty(1, 0), 16384).expect("syncs");
        assert_eq!(logs.marked().land(8192), 4096);
        assert_eq!(logs.marked().land(12288), 12288);
        assert_eq!(logs.marked().land(16384), 12288);
        logs.sync(slot(2), &empty(2, 5), 20480).expect("syncs");
        for offset in [4096, 8192, 12288, 16384, 20480] {
            assert_eq!(logs.marked().land(offset), 0, "a landing at {offset}");
        }
        assert_eq!(logs.marked().land(0), 0);
        assert_eq!(logs.marked().land(24576), 24576);
    }

    /// A trim at the landing of a range keeps it, and a trim past its newest record
    /// drops it. Here only memory holds a passed range, so the test reads `marked`;
    /// `marked_trims_take_no_allocation` of the `alloc` test sees it from outside.
    #[test]
    fn a_range_leaves_when_a_trim_passes_its_newest_record() {
        let records = [
            (4096, header(1, Path::Live, 0, 0, None)),
            (8192, header(1, Path::Live, 0, 0, None)),
            (12288, header(1, Path::Live, 0, 0, None)),
        ];
        let mut logs = walk_from(&records, 0);
        logs.hide(4096);
        assert_eq!(logs.marked().land(12288), 4096);
        assert_eq!(logs.marked.0, [(4096, 12288)]);
        logs.carry(slot(1), &carried(0, 3, None), 16384)
            .expect("the walk met the end");
        assert_eq!(logs.kept(), Some(16384));
        assert_eq!(logs.marked().land(16384), 16384);
        logs.hide(16384);
        assert_eq!(logs.marked.0, []);
    }

    #[test]
    fn a_mark_that_covers_an_older_range_takes_its_start() {
        let mut marked = Marked::default();
        marked.mark(8192, 12288);
        marked.mark(4096, 16384);
        for offset in [8192, 12288, 16384] {
            assert_eq!(marked.land(offset), 4096, "a landing at {offset}");
        }
        assert_eq!(marked.land(20480), 20480);
    }

    #[test]
    fn a_mark_from_the_end_of_a_range_merges_with_it() {
        let mut marked = Marked::default();
        marked.mark(4096, 8192);
        marked.mark(8192, 12288);
        assert_eq!(marked.land(12288), 4096);
    }

    #[test]
    fn a_range_inside_the_back_range_adds_no_range() {
        let mut marked = Marked::default();
        marked.mark(4096, 12288);
        marked.mark(8192, 12288);
        marked.mark(4096, 12288);
        assert_eq!(marked.land(4096), 4096);
        assert_eq!(marked.land(8192), 4096);
        assert_eq!(marked.land(12288), 4096);
        assert_eq!(marked.land(16384), 16384);
    }

    #[test]
    fn a_wider_range_at_the_record_of_the_back_range_merges() {
        let mut marked = Marked::default();
        marked.mark(8192, 12288);
        marked.mark(4096, 12288);
        assert_eq!(marked.land(8192), 4096);
        assert_eq!(marked.land(12288), 4096);
        assert_eq!(marked.land(16384), 16384);
    }

    #[test]
    fn a_mark_after_a_gap_keeps_two_ranges() {
        let mut marked = Marked::default();
        marked.mark(4096, 8192);
        marked.mark(12288, 16384);
        assert_eq!(marked.land(8192), 4096);
        assert_eq!(marked.land(12288), 12288);
        assert_eq!(marked.land(16384), 12288);
    }

    #[test]
    fn a_hide_drops_a_range_that_ends_before_the_tail() {
        let mut marked = Marked::default();
        marked.mark(4096, 8192);
        marked.hide(8192);
        assert_eq!(marked.land(8192), 4096);
        marked.hide(12288);
        assert_eq!(marked.land(8192), 8192);
    }

    #[test]
    fn a_hide_keeps_each_range_that_ends_at_or_after_the_tail() {
        let mut marked = Marked::default();
        marked.mark(4096, 8192);
        marked.mark(12288, 16384);
        marked.hide(4096);
        assert_eq!(marked.land(8192), 4096);
        marked.hide(12288);
        assert_eq!(marked.land(8192), 8192);
        assert_eq!(marked.land(16384), 12288);
    }

    /// The bytes that the runs, the items, and the ranges of `logs` hold. No call of
    /// `Logs` shows the room of its deques, so the tests of S4 read it.
    fn held(logs: &Logs) -> (usize, usize, usize) {
        let runs: usize = logs
            .paths
            .iter()
            .map(|log| log.runs.capacity() * size_of::<Run>())
            .sum();
        let items = logs.items.places.capacity() * size_of::<u32>()
            + logs.items.records.capacity() * size_of::<(u64, u64)>();
        let marked = logs.marked.0.capacity() * size_of::<(u64, u64)>();
        (runs, items, marked)
    }

    /// The bound of S4 on the bytes that [`held`] counts in a ring of `records`
    /// one-block records with `paths` paths: "at most 488/51 of the area, plus 48 bytes
    /// per record", and the room of the runs of each path with no run, 96 bytes.
    fn s4_bound(records: u64, paths: u64) -> u64 {
        4096 * records * 488 / 51 + 48 * records + 96 * paths
    }

    /// 65 one-block data records each hold an entry with no samples at seq 0 for each
    /// of 80 paths: 80 * 51 + 13 bytes fit in 4096.
    #[test]
    fn marked_runs_keep_memory_in_the_s4_bound() {
        let records: u64 = 65;
        let paths: u32 = 80;
        let mut logs = Logs::new(4096 * records);
        for record in 0..records {
            let offset = 4096 * (record + 1);
            for index in 1..=paths {
                let entry = header(index, Path::Live, 0, 0, None);
                logs.append(slot(index), &entry).expect("appends");
                logs.sync(slot(index), &entry, offset).expect("syncs");
            }
        }
        let (runs, items, marked) = held(&logs);
        let held = u64::try_from(runs + items + marked).expect("fits");
        let bound = s4_bound(records, u64::from(paths));
        assert!(
            held <= bound,
            "runs {runs} + items {items} + marked {marked} = {held} bytes, over the \
             bound of {bound} for {records} records"
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
                let entry = header(index, Path::Live, 0, 1, Some(1));
                logs.append(slot(index), &entry).expect("appends");
                logs.sync(slot(index), &entry, 4096).expect("syncs");
            }
            let (runs, items, marked) = held(&logs);
            let total = u64::try_from(runs + items + marked).expect("fits");
            let bound = s4_bound(1, u64::from(paths));
            assert!(
                total <= bound,
                "runs {runs} + items {items} + marked {marked} = {total} bytes, over \
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
                        let entry = header(index, Path::Live, first, 1, Some(1));
                        logs.append(at, &entry).expect("appends");
                        logs.sync(at, &entry, offset).expect("syncs");
                    }
                }
                offset += 4096;
                let mut tails = Carried::default();
                logs.idle(offset, usize::MAX, &mut tails);
                for tail in tails.tails() {
                    let index = (1..=groups * paths)
                        .find(|&i| channel::Key::from_u128(u128::from(i)) == tail.index)
                        .expect("a fed index");
                    logs.carry(slot(index), tail, offset).expect("carries");
                }
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
            let (runs, items, marked) = held(&logs);
            let total = u64::try_from(runs + items + marked).expect("fits");
            let bound = s4_bound(ring_records, u64::from(groups * paths));
            assert!(
                total <= bound,
                "runs {runs} + items {items} + marked {marked} = {total} bytes, over \
                 the bound of {bound} for a ring of {ring_records} records"
            );
        }
    }

    /// Logs of a ring of `area` bytes, with one path that has a run in each of the
    /// records 1 to `count`, of a block each.
    fn one_path_with_runs(area: u64, count: u64) -> Logs {
        let mut logs = Logs::new(area);
        for record in 1..=count {
            let entry = header(1, Path::Live, record - 1, 1, Some(1));
            logs.append(slot(1), &entry).expect("appends");
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

    #[test]
    fn a_trim_inside_a_lap_keeps_the_room() {
        let mut log = log_with(16, 3);
        log.trim(4096, AREA);
        assert_eq!(room_of(&log), (16, 2));
        assert_eq!((log.lap, log.peak), (0, 3));
    }

    #[test]
    fn the_trim_that_ends_a_lap_keeps_the_room_of_runs_that_fill_a_quarter_of_it() {
        let mut log = log_with(16, 4);
        log.trim(AREA, AREA);
        assert_eq!(room_of(&log), (16, 4));
        assert_eq!((log.lap, log.peak), (AREA, 0));
    }

    /// The lap met 5 runs, though 1 is left at its end.
    #[test]
    fn the_trim_that_ends_a_lap_shrinks_the_room_to_twice_the_most_runs_of_the_lap() {
        let mut log = log_with(32, 5);
        for record in 1..=4 {
            log.trim(4096 * record, AREA);
        }
        assert_eq!(room_of(&log), (32, 1));
        log.trim(AREA, AREA);
        assert_eq!(room_of(&log), (10, 1));
        assert_eq!((log.lap, log.peak), (AREA, 0));
    }

    #[test]
    fn the_trim_that_ends_a_lap_with_no_run_keeps_room_for_two() {
        let mut log = log_with(8, 1);
        log.trim(4096, AREA);
        log.trim(AREA, AREA);
        assert_eq!(room_of(&log), (2, 0));
    }

    #[test]
    fn a_lap_ends_at_the_first_trim_an_area_after_its_start() {
        let mut log = log_with(16, 0);
        log.lap = 4096;
        log.trim(AREA, AREA);
        assert_eq!((room_of(&log), log.lap), ((16, 0), 4096));
        log.trim(AREA + 4096, AREA);
        assert_eq!((room_of(&log), log.lap), ((2, 0), AREA + 4096));
    }

    /// The lap and the peak fit in the padding of a log, and a bucket of the map of
    /// places holds 12 bytes. S4 counts both.
    #[test]
    fn a_log_takes_144_bytes_and_its_place_12() {
        assert_eq!(size_of::<Log>(), 144);
        assert_eq!(size_of::<((Slot, Path), u32)>(), 12);
    }

    /// Gives the tail of each idle path a carry record at `offset`, as a commit does.
    fn carry_idle(logs: &mut Logs, offset: u64, indexes: u32) {
        let mut tails = Carried::default();
        logs.idle(offset, usize::MAX, &mut tails);
        for tail in tails.tails() {
            let index = (1..=indexes)
                .find(|&i| channel::Key::from_u128(u128::from(i)) == tail.index)
                .expect("a fed index");
            logs.carry(slot(index), tail, offset).expect("carries");
        }
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
            carry_idle(&mut logs, 4096 * record, 1);
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
        carry_idle(&mut logs, 4096 * 9, 1);
        logs.hide(4096 * 9);
        assert_eq!(room(&logs), (2, 0));
        let entry = header(1, Path::Live, 8, 1, Some(1));
        logs.append(slot(1), &entry).expect("appends");
        logs.sync(slot(1), &entry, 4096 * 10).expect("syncs");
        assert_eq!(room(&logs), (2, 1));
        assert_eq!(
            logs.runs(slot(1), Path::Live).collect::<Vec<_>>(),
            [run(8, 0, 4096 * 10)]
        );
    }

    /// The runs that each path held after each step, and the step before the trim
    /// that ended each of its laps.
    #[derive(Default)]
    struct Laps {
        held: Vec<usize>,
        ends: Vec<usize>,
        lap: u64,
    }

    proptest! {
        /// Records of a block each hold an entry of some of 8 paths, or the carried
        /// tails of the idle paths, and a trim may follow each, up to the oldest
        /// holder, in a ring of 4 blocks. Each lap spans at least the area. After
        /// each trim, each path has room for at most four times the most runs that it
        /// held from the step before the end of the lap before the current one, or
        /// four with no run.
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
        ) {
            let area = 4 * 4096;
            let mut logs = Logs::new(area);
            let mut laps: Vec<Laps> = Vec::new();
            for (record, (indexes, carried, trim)) in (1..).zip(&records) {
                let offset = 4096 * record;
                if *carried {
                    carry_idle(&mut logs, offset, 8);
                } else {
                    for &index in indexes {
                        let at = slot(index);
                        let first = logs.appended(at, Path::Live).seq;
                        let entry = header(index, Path::Live, first, 1, Some(1));
                        prop_assert_eq!(logs.append(at, &entry), Ok(()));
                        prop_assert_eq!(logs.sync(at, &entry, offset), Ok(()));
                    }
                }
                laps.resize_with(logs.paths.len(), Laps::default);
                for (log, laps) in logs.paths.iter().zip(&mut laps) {
                    laps.held.push(log.runs.len());
                }
                let Some(trim) = trim else { continue };
                let Some(kept) = logs.kept() else { continue };
                logs.hide(logs.hidden.max((4096 * trim).min(kept)));
                for (log, laps) in logs.paths.iter().zip(&mut laps) {
                    if log.lap != laps.lap {
                        prop_assert!(log.lap >= laps.lap + area);
                        laps.ends.push(laps.held.len() - 1);
                        laps.lap = log.lap;
                    }
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
        /// Commits that carry the idle tails after their data record and trim to
        /// a landing at or below `kept`, then a walk of the records left, as the
        /// open does, gives each path its durable position and each read what it gets.
        #[test]
        fn a_walk_after_each_landing_gives_what_the_logs_give(
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
                    let header =
                        header(next.index, next.path, first, next.len, next.last);
                    prop_assert_eq!(logs.append(at, &header), Ok(()));
                    queued.push((next.index, header));
                }
                let mut carried = Carried::default();
                logs.idle(8192 * *reach.min(&number), *limit, &mut carried);
                for (index, header) in &queued {
                    prop_assert_eq!(logs.sync(slot(*index), header, synced_at), Ok(()));
                }
                if !queued.is_empty() {
                    ring.push(Written::Data(synced_at, queued));
                }
                if *carries && !carried.tails().is_empty() {
                    logs.carried(&carried, carried_at);
                    ring.push(Written::Carry(carried_at, carried.tails().to_vec()));
                }
                if let Some(trim) = trim {
                    let bound = logs.kept().unwrap_or(synced_at + 4096);
                    hidden = hidden.max(logs.marked().land((8192 * trim).min(bound)));
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
                        prop_assert_eq!(walked.carry(at, tail, *offset), Ok(()));
                    }
                }
                Written::Data(offset, entries) if *offset >= hidden => {
                    for (index, header) in entries {
                        let at = slot(*index);
                        prop_assert_eq!(walked.append(at, header), Ok(()));
                        prop_assert_eq!(walked.sync(at, header, *offset), Ok(()));
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
                at = at.after(header.first, header.len);
                (at > from).then_some((header, at))
            })
            .collect()
    }
}
