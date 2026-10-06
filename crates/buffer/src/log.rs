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

/// A record that holds entries of one path: the `first` of the path's first
/// entry in it, and the record's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) first: u64,
    pub(crate) offset: u64,
}

/// One path of an index: where it stands with every appended entry, where it
/// stands on disk, and the records that hold it, oldest first.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Log {
    appended: Tail,
    durable: Tail,
    runs: VecDeque<Run>,
}

impl Log {
    /// Moves the durable tail past the entry and adds the record at `offset` to
    /// the runs when it is not the newest.
    fn sync(&mut self, header: &Header, offset: u64) -> Result<(), Invalid> {
        self.durable.advance(header)?;
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
        self.runs.push_back(Run {
            first: header.first,
            offset,
        });
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
    #[cfg_attr(not(test), expect(dead_code, reason = "a read starts from the runs"))]
    pub(crate) fn runs(&self, slot: Slot, path: Path) -> impl Iterator<Item = Run> {
        self.0
            .get(&(slot, path))
            .into_iter()
            .flat_map(|log| log.runs.iter().copied())
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
        self.change((slot, header.path), |log| log.appended.advance(header))
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
        self.change((slot, header.path), |log| log.sync(header, offset))
    }

    /// Applies `change` to the log at `key`, which starts empty when the path is
    /// new. A change that fails on a new path adds nothing.
    fn change(
        &mut self,
        key: (Slot, Path),
        change: impl FnOnce(&mut Log) -> Result<(), Invalid>,
    ) -> Result<(), Invalid> {
        if let Some(log) = self.0.get_mut(&key) {
            return change(log);
        }
        let mut log = Log::default();
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

    fn run(first: u64, offset: u64) -> Run {
        Run { first, offset }
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

    /// The entries of one path, in log order.
    fn on(fed: &Fed, slot: Slot, path: Path) -> impl Iterator<Item = &Header> {
        fed.iter()
            .filter(move |(at, header, _)| (*at, header.path) == (slot, path))
            .map(|(_, header, _)| header)
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
    /// entry of it, with the `first` of its first entry there.
    fn expected(fed: &Fed, slot: Slot, path: Path) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        for (at, header, offset) in fed {
            if (*at, header.path) != (slot, path) {
                continue;
            }
            if runs.last().is_none_or(|run| run.offset != *offset) {
                runs.push(run(header.first, *offset));
            }
        }
        runs
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
                    prop_assert_eq!(runs, expected(&fed, slot(index), path));
                    prop_assert_eq!(logs.appended(slot(index), path), tail(&fed, slot(index), path));
                    prop_assert_eq!(logs.durable(slot(index), path), tail(&fed, slot(index), path));
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
        assert_eq!(live, [run(0, 4096), run(4, 8192)]);
        let backfill: Vec<Run> = logs.runs(slot(1), Path::Backfill).collect();
        assert_eq!(backfill, [run(0, 4096)]);
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
}
