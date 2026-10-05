//! Where each path stands on disk, and which records hold its entries.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::collections::VecDeque;

use types::channel::Slot;
use types::frame::Path;
use types::hash;

use crate::entry::Header;
use crate::tails::{Invalid, Tail, Tails};

/// A record that holds entries of one path: the `first` of the path's first
/// entry in it, and the record's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) first: u64,
    pub(crate) offset: u64,
}

/// The durable log of one path: its tail and the records that hold it, oldest
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Log {
    tail: Tail,
    runs: VecDeque<Run>,
}

/// The durable log of every path, by the node's slot of the index and the path.
/// The recovery walk and each sync feed it one entry at a time, in ring order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Logs(hash::Map<(Slot, Path), Log>);

impl Logs {
    /// The tail of `path` of the index at `slot`: `Tail::default()` before its
    /// first entry.
    pub(crate) fn tail(&self, slot: Slot, path: Path) -> Tail {
        self.0.get(&(slot, path)).map_or_default(|log| log.tail)
    }

    /// The records that hold `path` of the index at `slot`, oldest first.
    #[cfg_attr(not(test), expect(dead_code, reason = "a read starts from the runs"))]
    pub(crate) fn runs(&self, slot: Slot, path: Path) -> impl Iterator<Item = Run> {
        self.0
            .get(&(slot, path))
            .into_iter()
            .flat_map(|log| log.runs.iter().copied())
    }

    /// A copy of the tail of every path.
    pub(crate) fn tails(&self) -> Tails {
        self.0.iter().map(|(&key, log)| (key, log.tail)).collect()
    }

    /// Moves the tail of the header's path past the entry, as [`Tails::advance`],
    /// and adds the record at `offset` to the path's runs when it is not the
    /// newest.
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Tails::advance`]. Nothing changes.
    ///
    /// # Panics
    ///
    /// When `offset` is below the newest run: records come in ring order.
    pub(crate) fn advance(
        &mut self,
        slot: Slot,
        header: &Header,
        offset: u64,
    ) -> Result<(), Invalid> {
        let log = self.0.entry((slot, header.path)).or_default();
        log.tail.advance(header)?;
        if let Some(newest) = log.runs.back() {
            assert!(
                newest.offset <= offset,
                "invariant: record {offset} comes after record {}",
                newest.offset
            );
            if newest.offset == offset {
                return Ok(());
            }
        }
        log.runs.push_back(Run {
            first: header.first,
            offset,
        });
        Ok(())
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use types::channel;
    use types::time::Stamp;

    fn slot(value: u32) -> Slot {
        Slot::new(value)
    }

    fn header(index: u32, path: Path, first: u64, len: u32) -> Header {
        Header {
            index: channel::Key::from_u128(u128::from(index)),
            path,
            first,
            len,
            stored_at: Stamp::from_nanos(7),
            last: Some(Stamp::from_nanos(9)),
            tag: 0,
            bytes: 0,
        }
    }

    fn run(first: u64, offset: u64) -> Run {
        Run { first, offset }
    }

    /// One entry of a model record: which path it is on, how far past the tail
    /// it starts, and how long it is.
    #[derive(Clone, Debug)]
    struct Next {
        index: u32,
        path: Path,
        skip: u64,
        len: u32,
    }

    fn next() -> impl Strategy<Value = Next> {
        (
            0..3u32,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..3u64,
            0..4u32,
        )
            .prop_map(|(index, path, skip, len)| Next {
                index,
                path,
                skip,
                len,
            })
    }

    /// The entries of the model, each with the offset of its record.
    type Fed = Vec<(Slot, Header, u64)>;

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
        /// The runs follow the model, and the tails follow `Tails`.
        #[test]
        fn follows_a_model_per_path(
            records in prop::collection::vec(prop::collection::vec(next(), 1..5), 0..12),
        ) {
            let mut logs = Logs::default();
            let mut tails = Tails::default();
            let mut fed: Fed = Vec::new();
            for (number, record) in records.iter().enumerate() {
                let offset = 4096 * (u64::try_from(number).expect("small") + 1);
                for next in record {
                    let at = slot(next.index);
                    let first = tails.get(at, next.path).seq + next.skip;
                    let header = header(next.index, next.path, first, next.len);
                    prop_assert_eq!(tails.advance(at, &header), Ok(()));
                    prop_assert_eq!(logs.advance(at, &header, offset), Ok(()));
                    fed.push((at, header, offset));
                }
            }
            for index in 0..3 {
                for path in [Path::Live, Path::Backfill] {
                    let runs: Vec<Run> = logs.runs(slot(index), path).collect();
                    prop_assert_eq!(runs, expected(&fed, slot(index), path));
                    prop_assert_eq!(logs.tail(slot(index), path), tails.get(slot(index), path));
                    prop_assert_eq!(logs.tails().get(slot(index), path), tails.get(slot(index), path));
                }
            }
        }
    }

    #[test]
    fn a_record_with_two_entries_of_a_path_makes_one_run() {
        let mut logs = Logs::default();
        logs.advance(slot(1), &header(1, Path::Live, 0, 2), 4096)
            .expect("advances");
        logs.advance(slot(1), &header(1, Path::Live, 2, 2), 4096)
            .expect("advances");
        logs.advance(slot(1), &header(1, Path::Backfill, 0, 1), 4096)
            .expect("advances");
        logs.advance(slot(1), &header(1, Path::Live, 4, 0), 8192)
            .expect("advances");
        let live: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(live, [run(0, 4096), run(4, 8192)]);
        let backfill: Vec<Run> = logs.runs(slot(1), Path::Backfill).collect();
        assert_eq!(backfill, [run(0, 4096)]);
        assert_eq!(logs.tail(slot(1), Path::Live).seq, 4);
        assert_eq!(logs.runs(slot(2), Path::Live).count(), 0);
        assert_eq!(logs.tail(slot(2), Path::Live), Tail::default());
    }

    #[test]
    fn an_invalid_entry_changes_nothing() {
        let mut logs = Logs::default();
        logs.advance(slot(1), &header(1, Path::Live, 0, 3), 4096)
            .expect("advances");
        let invalid = Invalid::Below {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: 2,
            tail: 3,
        };
        assert_eq!(
            logs.advance(slot(1), &header(1, Path::Live, 2, 1), 8192),
            Err(invalid)
        );
        let live: Vec<Run> = logs.runs(slot(1), Path::Live).collect();
        assert_eq!(live, [run(0, 4096)]);
        assert_eq!(logs.tail(slot(1), Path::Live).seq, 3);
    }

    #[test]
    #[should_panic(expected = "invariant: record 4096 comes after record 8192")]
    fn a_record_before_the_newest_run_is_a_broken_invariant() {
        let mut logs = Logs::default();
        logs.advance(slot(1), &header(1, Path::Live, 0, 3), 8192)
            .expect("advances");
        logs.advance(slot(1), &header(1, Path::Live, 3, 1), 4096)
            .expect("the invariant panics first");
    }
}
