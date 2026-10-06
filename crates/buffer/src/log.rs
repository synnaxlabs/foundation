//! Where each path of each index stands: with every appended entry, on disk, and
//! in which records.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::collections::VecDeque;
use std::fmt;

use types::channel::{self, Slot};
use types::frame::Path;
use types::hash;
use types::time::Stamp;

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
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
    /// When `first + len` passes `u64::MAX`: the tail checked the entry.
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
            given: given.saturating_add(1),
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

/// One path of an index: where it stands with every appended entry, where it
/// stands on disk, and the records that hold it, oldest first.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Log {
    index: channel::Key,
    appended: Tail,
    durable: Tail,
    /// The mark after the newest durable entry.
    mark: Mark,
    runs: VecDeque<Run>,
}

impl Log {
    fn new(index: channel::Key) -> Self {
        Self {
            index,
            appended: Tail::default(),
            durable: Tail::default(),
            mark: Mark::default(),
            runs: VecDeque::new(),
        }
    }

    /// Moves the durable tail and mark past the entry and adds the record at
    /// `offset` to the runs when it is not the newest.
    fn sync(&mut self, header: &Header, offset: u64) -> Result<(), Invalid> {
        self.durable.advance(header)?;
        let start = self.mark;
        self.mark = start.after(header.first, header.len);
        if let Some(newest) = self.runs.back() {
            assert!(
                newest.offset <= offset,
                "invariant: record {offset} comes after record {}",
                newest.offset
            );
            if newest.offset == offset {
                return Ok(());
            }
        }
        self.runs.push_back(Run { start, offset });
        Ok(())
    }
}

/// Every path the buffer holds, by the node's slot of the index and the path.
/// `append` feeds it with each appended entry; the recovery walk and each sync
/// feed it with each record's entries, in ring order.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct Logs(hash::Map<(Slot, Path), Log>);

impl Logs {
    /// Where `path` of the index at `slot` stands with every appended entry:
    /// `Tail::default()` before its first entry.
    pub(crate) fn appended(&self, slot: Slot, path: Path) -> Tail {
        self.0.get(&(slot, path)).map_or_default(|log| log.appended)
    }

    /// Where `path` of the index at `slot` stands on disk: `Tail::default()`
    /// before its first synced entry.
    pub(crate) fn durable(&self, slot: Slot, path: Path) -> Tail {
        self.0.get(&(slot, path)).map_or_default(|log| log.durable)
    }

    /// The records that hold `path` of the index at `slot`, oldest first.
    #[cfg(test)]
    pub(crate) fn runs(&self, slot: Slot, path: Path) -> impl Iterator<Item = Run> {
        self.0
            .get(&(slot, path))
            .into_iter()
            .flat_map(|log| log.runs.iter().copied())
    }

    /// The oldest record with a durable entry of `path` of the index at `slot`
    /// that ends past `from`, with the index, or `None` when no entry does.
    pub(crate) fn run(
        &self,
        slot: Slot,
        path: Path,
        from: Mark,
    ) -> Option<(channel::Key, Run)> {
        let log = self.0.get(&(slot, path))?;
        let after = log.runs.partition_point(|run| run.start <= from);
        let run = *log.runs.get(after.saturating_sub(1))?;
        let end = log
            .runs
            .get(after.max(1))
            .map_or(log.mark, |next| next.start);
        (end > from).then_some((log.index, run))
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
    }

    /// Moves the durable tail of the header's path past the entry, as
    /// [`Tail::advance`], and adds the record at `offset` to the path's runs when
    /// it is not the newest.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Tail::advance`]. Nothing changes.
    ///
    /// # Panics
    ///
    /// When `offset` is below the newest run: records come in ring order.
    pub(crate) fn sync(
        &mut self,
        slot: Slot,
        header: &Header,
        offset: u64,
    ) -> Result<(), Invalid> {
        self.change(slot, header, |log| log.sync(header, offset))
    }

    /// Applies `change` to the log of the header's path, which starts empty when
    /// the path is new. A change that fails on a new path adds nothing.
    fn change(
        &mut self,
        slot: Slot,
        header: &Header,
        change: impl FnOnce(&mut Log) -> Result<(), Invalid>,
    ) -> Result<(), Invalid> {
        let key = (slot, header.path);
        if let Some(log) = self.0.get_mut(&key) {
            return change(log);
        }
        let mut log = Log::new(header.index);
        change(&mut log)?;
        self.0.insert(key, log);
        Ok(())
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
            let mut logs = Logs::default();
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
                        let found = runs
                            .iter()
                            .rev()
                            .find(|run| run.offset == *offset)
                            .map(|run| (key, *run));
                        prop_assert_eq!(logs.run(slot(index), path, *before), found);
                        end = before.after(header.first, header.len);
                    }
                    prop_assert_eq!(logs.run(slot(index), path, end), None);
                }
            }
        }
    }

    #[test]
    fn a_new_path_stands_at_seq_zero_with_no_stamp() {
        let logs = Logs::default();
        assert_eq!(logs.appended(slot(1), Path::Live), Tail::default());
        assert_eq!(logs.durable(slot(1), Path::Live), Tail::default());
        assert_eq!(logs.runs(slot(1), Path::Live).count(), 0);
    }

    #[test]
    fn each_path_of_an_index_has_its_own_tail() {
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        assert_eq!(logs, Logs::default());
    }

    #[test]
    fn a_record_with_two_entries_of_a_path_makes_one_run() {
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        let mut logs = Logs::default();
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
        assert_eq!(logs, Logs::default());
    }

    #[test]
    #[should_panic(expected = "invariant: record 4096 comes after record 8192")]
    fn a_record_before_the_newest_run_is_a_broken_invariant() {
        let mut logs = Logs::default();
        logs.sync(slot(1), &header(1, Path::Live, 0, 3, None), 8192)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 3, 1, None), 4096)
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
        let _ = mark(0, 0).after(u64::MAX, 1);
    }

    /// Record 1 holds [2, 5) and an entry with no samples at 5; record 2 starts at
    /// 5. A read from 5 starts in record 1, for the entry with no samples.
    #[test]
    fn a_read_from_the_seq_an_empty_entry_stands_at_starts_in_its_record() {
        let mut logs = Logs::default();
        logs.sync(slot(1), &header(1, Path::Live, 2, 3, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 5, 0, None), 4096)
            .expect("syncs");
        logs.sync(slot(1), &header(1, Path::Live, 5, 2, None), 8192)
            .expect("syncs");
        let key = channel::Key::from_u128(1);
        let first = (key, run(0, 0, 4096));
        let second = (key, run(5, 1, 8192));
        assert_eq!(logs.run(slot(1), Path::Live, mark(0, 0)), Some(first));
        assert_eq!(logs.run(slot(1), Path::Live, mark(3, 0)), Some(first));
        assert_eq!(logs.run(slot(1), Path::Live, mark(5, 0)), Some(first));
        assert_eq!(logs.run(slot(1), Path::Live, mark(5, 1)), Some(second));
        assert_eq!(logs.run(slot(1), Path::Live, mark(6, 0)), Some(second));
        assert_eq!(logs.run(slot(1), Path::Live, mark(7, 0)), None);
        assert_eq!(logs.run(slot(1), Path::Live, mark(7, 3)), None);
        assert_eq!(logs.run(slot(1), Path::Backfill, mark(0, 0)), None);
        assert_eq!(logs.run(slot(2), Path::Live, mark(0, 0)), None);
    }
}
