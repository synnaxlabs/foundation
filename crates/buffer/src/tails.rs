//! Where each path of each index stands in the log.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

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

/// The tails of every path the log holds, by the node's slot of the index and
/// the path.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tails(hash::Map<(Slot, Path), Tail>);

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

impl Tails {
    /// The tail of `path` of the index at `slot`: `Tail::default()` before its
    /// first entry.
    pub(crate) fn get(&self, slot: Slot, path: Path) -> Tail {
        self.0.get(&(slot, path)).copied().unwrap_or_default()
    }

    /// Moves the tail of the header's path past the entry, as [`Tail::advance`].
    ///
    /// # Errors
    ///
    /// [`Invalid`] as [`Tail::advance`]. The tail does not move.
    pub(crate) fn advance(
        &mut self,
        slot: Slot,
        header: &Header,
    ) -> Result<(), Invalid> {
        self.0
            .entry((slot, header.path))
            .or_default()
            .advance(header)
    }
}

impl FromIterator<((Slot, Path), Tail)> for Tails {
    fn from_iter<I: IntoIterator<Item = ((Slot, Path), Tail)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
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

    /// The entries of one path, in log order.
    fn on(entries: &[Header], index: u32, path: Path) -> impl Iterator<Item = &Header> {
        entries.iter().filter(move |header| {
            header.index == channel::Key::from_u128(u128::from(index))
                && header.path == path
        })
    }

    /// The seq past the newest entry of one path: zero before its first entry.
    fn end(entries: &[Header], index: u32, path: Path) -> u64 {
        on(entries, index, path)
            .map(|header| header.first + u64::from(header.len))
            .max()
            .unwrap_or(0)
    }

    proptest! {
        #[test]
        fn follows_a_model_per_index_and_path(log in prop::collection::vec(next(), 0..40)) {
            let mut tails = Tails::default();
            let mut entries: Vec<Header> = Vec::new();
            for next in log {
                let first = end(&entries, next.index, next.path) + next.skip;
                let entry = header(next.index, next.path, first, next.len, next.last);
                prop_assert_eq!(tails.advance(slot(next.index), &entry), Ok(()));
                entries.push(entry);
                let stamps: Vec<Stamp> =
                    on(&entries, next.index, next.path).filter_map(|header| header.last).collect();
                let expected = Tail {
                    seq: end(&entries, next.index, next.path),
                    stamp: stamps.last().copied(),
                };
                prop_assert_eq!(tails.get(slot(next.index), next.path), expected);
            }
            for index in 0..3 {
                for path in [Path::Live, Path::Backfill] {
                    if on(&entries, index, path).next().is_none() {
                        prop_assert_eq!(tails.get(slot(index), path), Tail::default());
                    }
                }
            }
        }
    }

    #[test]
    fn a_new_path_stands_at_seq_zero_with_no_stamp() {
        let tails = Tails::default();
        assert_eq!(tails.get(slot(1), Path::Live), Tail::default());
    }

    #[test]
    fn each_path_of_an_index_has_its_own_tail() {
        let mut tails = Tails::default();
        tails
            .advance(slot(1), &header(1, Path::Live, 0, 3, Some(30)))
            .expect("advances");
        tails
            .advance(slot(1), &header(1, Path::Backfill, 0, 1, Some(10)))
            .expect("advances");
        tails
            .advance(slot(2), &header(2, Path::Live, 0, 5, None))
            .expect("advances");
        let live = Tail {
            seq: 3,
            stamp: Some(Stamp::from_nanos(30)),
        };
        let backfill = Tail {
            seq: 1,
            stamp: Some(Stamp::from_nanos(10)),
        };
        assert_eq!(tails.get(slot(1), Path::Live), live);
        assert_eq!(tails.get(slot(1), Path::Backfill), backfill);
        assert_eq!(
            tails.get(slot(2), Path::Live),
            Tail {
                seq: 5,
                stamp: None
            }
        );
        assert_eq!(tails.get(slot(2), Path::Backfill), Tail::default());
    }

    #[test]
    fn a_skip_ahead_moves_the_seq_past_the_gap() {
        let mut tails = Tails::default();
        tails
            .advance(slot(1), &header(1, Path::Live, 0, 2, Some(2)))
            .expect("advances");
        tails
            .advance(slot(1), &header(1, Path::Live, 10, 1, Some(11)))
            .expect("advances");
        let expected = Tail {
            seq: 11,
            stamp: Some(Stamp::from_nanos(11)),
        };
        assert_eq!(tails.get(slot(1), Path::Live), expected);
    }

    #[test]
    fn an_entry_with_no_last_stamp_keeps_the_stamp() {
        let mut tails = Tails::default();
        tails
            .advance(slot(1), &header(1, Path::Live, 0, 2, Some(2)))
            .expect("advances");
        tails
            .advance(slot(1), &header(1, Path::Live, 2, 0, None))
            .expect("advances");
        let expected = Tail {
            seq: 2,
            stamp: Some(Stamp::from_nanos(2)),
        };
        assert_eq!(tails.get(slot(1), Path::Live), expected);
    }

    #[test]
    fn an_entry_below_the_tail_is_invalid_and_leaves_the_tail() {
        let mut tails = Tails::default();
        tails
            .advance(slot(1), &header(1, Path::Live, 0, 3, None))
            .expect("advances");
        let invalid = Invalid::Below {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: 2,
            tail: 3,
        };
        assert_eq!(
            tails.advance(slot(1), &header(1, Path::Live, 2, 1, Some(9))),
            Err(invalid)
        );
        assert_eq!(
            invalid.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live \
             starts at 2, below the tail 3"
        );
        assert_eq!(
            tails.get(slot(1), Path::Live),
            Tail {
                seq: 3,
                stamp: None
            }
        );
    }

    #[test]
    fn an_entry_past_the_last_seq_is_invalid_and_leaves_the_tail() {
        let mut tails = Tails::default();
        let invalid = Invalid::Past {
            index: channel::Key::from_u128(1),
            path: Path::Live,
            first: u64::MAX,
            len: 1,
        };
        assert_eq!(
            tails.advance(slot(1), &header(1, Path::Live, u64::MAX, 1, Some(9))),
            Err(invalid)
        );
        assert_eq!(
            invalid.to_string(),
            "an entry of index 00000000-0000-0000-0000-000000000001 on path Live \
             starts at 18446744073709551615 with 1 samples, past the last seq"
        );
        assert_eq!(tails.get(slot(1), Path::Live), Tail::default());
    }
}
