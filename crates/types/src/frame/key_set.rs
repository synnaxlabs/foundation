//! Key sets: the channels that a writer session sends, interned once per session.

use std::iter;
use std::sync::Arc;

use crate::channel::Slot;
use crate::hash;
use crate::sample::{Scalar, Type};

/// A key set's number in its node's [`Interner`]: dense from 0 and node-local, never
/// sent on the wire or stored on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u32);

impl Key {
    /// The key set number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The channels that a writer session sends, sorted by slot, with each entry's type and
/// index group. Every frame of the session points at it. Only an [`Interner`] makes one.
#[derive(Debug)]
pub struct KeySet {
    key: Key,
    entries: Arc<[Entry]>,
    groups: Arc<[usize]>,
}

impl KeySet {
    /// The key set's number in its interner.
    #[must_use]
    pub const fn key(&self) -> Key {
        self.key
    }

    /// The entries, sorted by slot.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The position in [`Self::entries`] of each group's index. Group numbers follow
    /// the order of the index slots.
    #[must_use]
    pub fn groups(&self) -> &[usize] {
        &self.groups
    }

    /// The position of `slot` in [`Self::entries`], or `None` when the key set does not
    /// hold it.
    #[must_use]
    pub fn find(&self, slot: Slot) -> Option<usize> {
        self.entries
            .binary_search_by_key(&slot, |entry| entry.slot)
            .ok()
    }
}

/// One channel of a key set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// The channel.
    pub slot: Slot,
    /// The layout of the channel's samples. An index has `Stamp`.
    pub data_type: Type,
    /// The entry's index group: a position in [`KeySet::groups`].
    pub group: u32,
}

/// One index and the data channels on it that a writer sends, as the `hub` gives them
/// at writer open.
#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    /// The index channel. Its samples are timestamps.
    pub index: Slot,
    /// Each data channel on the index, with the layout of its samples.
    pub data: &'a [(Slot, Type)],
}

/// The node's table of key sets. `node` makes one and injects it into `hub` and `home`,
/// which intern at session open. Methods take `&mut self`: the caller serializes them.
#[derive(Debug, Default)]
pub struct Interner {
    sets: hash::Map<Shape, Arc<KeySet>>,
    snapshot: Snapshot,
}

impl Interner {
    /// An interner with no key sets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The key set of `groups`: each index, with type `Stamp`, and each data channel.
    /// Groups are numbered in the order of their index slots. Equal groups, in any
    /// order, give the same key set. A new key set copies the snapshot's list, so it
    /// takes time linear in the number of key sets.
    ///
    /// # Panics
    ///
    /// If a slot appears twice: the `hub` builds groups from the spec, so that is a bug.
    /// Also if the node already holds 2^32 key sets.
    pub fn intern(&mut self, groups: &[Group<'_>]) -> Arc<KeySet> {
        let stamp = Type::Scalar(Scalar::Stamp);
        let mut channels: Vec<(Slot, Type, Slot)> = groups
            .iter()
            .flat_map(|g| {
                let data = g.data.iter().map(|&(slot, kind)| (slot, kind, g.index));
                iter::once((g.index, stamp, g.index)).chain(data)
            })
            .collect();
        channels.sort_unstable_by_key(|&(slot, ..)| slot);
        if let Some([(slot, ..), _]) =
            channels.array_windows().find(|[a, b]| a.0 == b.0)
        {
            panic!("slot {} appears twice in a key set", slot.get());
        }
        let mut indexes: Vec<Slot> = groups.iter().map(|g| g.index).collect();
        indexes.sort_unstable();
        let entries: Arc<[Entry]> = channels
            .iter()
            .map(|&(slot, data_type, index)| {
                let at = indexes.partition_point(|&other| other < index);
                let group =
                    u32::try_from(at).expect("invariant: index slots are distinct");
                Entry {
                    slot,
                    data_type,
                    group,
                }
            })
            .collect();
        let positions: Arc<[usize]> = indexes
            .iter()
            .map(|&index| {
                entries
                    .binary_search_by_key(&index, |entry| entry.slot)
                    .expect("invariant: each index is an entry")
            })
            .collect();
        let shape = (entries, positions);
        if let Some(set) = self.sets.get(&shape) {
            return Arc::clone(set);
        }
        let n =
            u32::try_from(self.sets.len()).expect("a node holds at most 2^32 key sets");
        let set = Arc::new(KeySet {
            key: Key(n),
            entries: Arc::clone(&shape.0),
            groups: Arc::clone(&shape.1),
        });
        let sets = self.snapshot.0.iter().cloned();
        self.snapshot = Snapshot(sets.chain([Arc::clone(&set)]).collect());
        self.sets.insert(shape, Arc::clone(&set));
        set
    }

    /// The key sets interned so far.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }
}

/// A key set's entries and the position of each group's index: what makes two key sets
/// equal.
type Shape = (Arc<[Entry]>, Arc<[usize]>);

/// The key sets of an [`Interner`] when it was taken. Cloning it is cheap, so each shard
/// keeps one and reads it without the interner.
#[derive(Clone, Debug, Default)]
pub struct Snapshot(Arc<[Arc<KeySet>]>);

impl Snapshot {
    /// The key set with `key`, or `None` when it was interned after this snapshot.
    #[must_use]
    pub fn get(&self, key: Key) -> Option<&Arc<KeySet>> {
        self.0.get(usize::try_from(key.get()).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const STAMP: Type = Type::Scalar(Scalar::Stamp);
    const F64: Type = Type::Scalar(Scalar::F64);
    const U8: Type = Type::Scalar(Scalar::U8);

    fn slot(n: u32) -> Slot {
        Slot::new(n)
    }

    fn entry(n: u32, data_type: Type, group: u32) -> Entry {
        Entry {
            slot: slot(n),
            data_type,
            group,
        }
    }

    #[test]
    fn sorts_entries_and_numbers_groups_by_index_slot() {
        let mut interner = Interner::new();
        let set = interner.intern(&[
            Group {
                index: slot(9),
                data: &[(slot(2), F64)],
            },
            Group {
                index: slot(4),
                data: &[(slot(7), U8), (slot(1), F64)],
            },
        ]);
        assert_eq!(
            set.entries(),
            [
                entry(1, F64, 0),
                entry(2, F64, 1),
                entry(4, STAMP, 0),
                entry(7, U8, 0),
                entry(9, STAMP, 1),
            ]
        );
        assert_eq!(set.groups(), [2, 4]);
        assert_eq!(set.key().get(), 0);
    }

    #[test]
    fn finds_the_entry_of_each_slot() {
        let mut interner = Interner::new();
        let set = interner.intern(&[Group {
            index: slot(5),
            data: &[(slot(8), F64), (slot(2), F64)],
        }]);
        assert_eq!(set.find(slot(2)), Some(0));
        assert_eq!(set.find(slot(5)), Some(1));
        assert_eq!(set.find(slot(8)), Some(2));
        assert_eq!(set.find(slot(3)), None);
    }

    #[test]
    fn gives_equal_groups_the_same_key_set() {
        let mut interner = Interner::new();
        let first = interner.intern(&[
            Group {
                index: slot(1),
                data: &[(slot(2), F64), (slot(3), U8)],
            },
            Group {
                index: slot(4),
                data: &[],
            },
        ]);
        let reordered = interner.intern(&[
            Group {
                index: slot(4),
                data: &[],
            },
            Group {
                index: slot(1),
                data: &[(slot(3), U8), (slot(2), F64)],
            },
        ]);
        let other = interner.intern(&[Group {
            index: slot(1),
            data: &[(slot(2), U8), (slot(3), U8)],
        }]);
        assert!(Arc::ptr_eq(&first, &reordered));
        assert_eq!(first.key().get(), 0);
        assert_eq!(other.key().get(), 1);
    }

    #[test]
    fn tells_an_index_from_a_stamp_channel_on_it() {
        let mut interner = Interner::new();
        let first = interner.intern(&[Group {
            index: slot(1),
            data: &[(slot(2), STAMP)],
        }]);
        let swapped = interner.intern(&[Group {
            index: slot(2),
            data: &[(slot(1), STAMP)],
        }]);
        assert_eq!(first.entries(), swapped.entries());
        assert_eq!(first.groups(), [0]);
        assert_eq!(swapped.groups(), [1]);
        assert_eq!(swapped.key().get(), 1);
    }

    #[test]
    fn tells_an_index_from_a_stamp_channel_beside_another_group() {
        let mut interner = Interner::new();
        let first = interner.intern(&[
            Group {
                index: slot(1),
                data: &[(slot(2), STAMP)],
            },
            Group {
                index: slot(5),
                data: &[],
            },
        ]);
        let swapped = interner.intern(&[
            Group {
                index: slot(2),
                data: &[(slot(1), STAMP)],
            },
            Group {
                index: slot(5),
                data: &[],
            },
        ]);
        assert_eq!(first.groups(), [0, 2]);
        assert_eq!(swapped.groups(), [1, 2]);
    }

    #[test]
    fn snapshots_hold_the_key_sets_interned_before_them() {
        let mut interner = Interner::new();
        let empty = interner.snapshot();
        let first = interner.intern(&[Group {
            index: slot(0),
            data: &[],
        }]);
        let one = interner.snapshot();
        let second = interner.intern(&[Group {
            index: slot(1),
            data: &[],
        }]);
        let two = interner.snapshot();
        assert!(empty.get(first.key()).is_none());
        assert!(Arc::ptr_eq(one.get(first.key()).unwrap(), &first));
        assert!(one.get(second.key()).is_none());
        assert!(Arc::ptr_eq(two.get(first.key()).unwrap(), &first));
        assert!(Arc::ptr_eq(two.get(second.key()).unwrap(), &second));
    }

    #[test]
    #[should_panic(expected = "slot 2 appears twice in a key set")]
    fn refuses_a_data_channel_twice() {
        Interner::new().intern(&[Group {
            index: slot(1),
            data: &[(slot(2), F64), (slot(2), U8)],
        }]);
    }

    #[test]
    #[should_panic(expected = "slot 1 appears twice in a key set")]
    fn refuses_an_index_that_is_also_data() {
        Interner::new().intern(&[
            Group {
                index: slot(1),
                data: &[],
            },
            Group {
                index: slot(3),
                data: &[(slot(1), STAMP)],
            },
        ]);
    }

    #[test]
    #[should_panic(expected = "slot 6 appears twice in a key set")]
    fn refuses_an_index_twice() {
        let group = Group {
            index: slot(6),
            data: &[],
        };
        Interner::new().intern(&[group, group]);
    }

    #[test]
    fn holds_more_groups_than_a_u16_counts() {
        let groups: Vec<Group<'_>> = (0..70_000)
            .map(|n| Group {
                index: slot(n),
                data: &[],
            })
            .collect();
        let set = Interner::new().intern(&groups);
        assert_eq!(set.groups().len(), 70_000);
        assert_eq!(set.entries().last(), Some(&entry(69_999, STAMP, 69_999)));
    }

    type Groups = Vec<(Slot, Vec<(Slot, Type)>)>;

    /// Up to 5 groups over distinct slots, with data channels of random types.
    fn groups() -> impl Strategy<Value = Groups> {
        prop::collection::btree_set(0_u32..1000, 1..40)
            .prop_map(|slots| slots.into_iter().collect::<Vec<_>>())
            .prop_shuffle()
            .prop_flat_map(|slots| {
                let n = slots.len();
                let kind = prop_oneof![Just(F64), Just(U8), Just(STAMP)];
                let picks = prop::collection::vec((0_usize..5, kind), n);
                (Just(slots), 1..=n.min(5), picks)
            })
            .prop_map(|(slots, count, picks)| {
                let (indexes, data) = slots.split_at(count);
                let mut groups: Groups =
                    indexes.iter().map(|&n| (slot(n), Vec::new())).collect();
                for (&n, &(group, kind)) in data.iter().zip(&picks) {
                    groups[group % count].1.push((slot(n), kind));
                }
                groups
            })
    }

    /// `groups` with each index swapped for the first `Stamp` data channel on it.
    fn swapped(groups: &Groups) -> Groups {
        let mut groups = groups.clone();
        for (index, data) in &mut groups {
            if let Some(channel) = data.iter_mut().find(|(_, kind)| *kind == STAMP) {
                std::mem::swap(index, &mut channel.0);
            }
        }
        groups
    }

    fn borrowed(groups: &Groups) -> Vec<Group<'_>> {
        groups
            .iter()
            .map(|(index, data)| Group {
                index: *index,
                data,
            })
            .collect()
    }

    /// Checks that `set` holds each channel of `groups` once, under its own index.
    fn check(set: &KeySet, groups: &Groups) -> Result<(), TestCaseError> {
        let entries = set.entries();
        prop_assert!(entries.is_sorted_by(|a, b| a.slot < b.slot));
        let total: usize = groups.iter().map(|(_, data)| 1 + data.len()).sum();
        prop_assert_eq!(entries.len(), total);
        prop_assert_eq!(set.groups().len(), groups.len());
        for (index, data) in groups {
            let at = set.find(*index).unwrap();
            let group = entries[at].group;
            let numbered = set.groups().iter().position(|&position| position == at);
            prop_assert_eq!(entries[at].data_type, STAMP);
            prop_assert_eq!(numbered.and_then(|g| u32::try_from(g).ok()), Some(group));
            for (slot, kind) in data {
                let entry = entries[set.find(*slot).unwrap()];
                prop_assert_eq!(entry.data_type, *kind);
                prop_assert_eq!(entry.group, group);
            }
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn holds_each_channel_once_under_its_index(groups in groups()) {
            let mut interner = Interner::new();
            let other = swapped(&groups);
            let first = interner.intern(&borrowed(&groups));
            let second = interner.intern(&borrowed(&other));
            check(&first, &groups)?;
            check(&second, &other)?;
        }

        #[test]
        fn interns_groups_in_any_order_to_one_key_set(groups in groups()) {
            let mut reversed = groups.clone();
            reversed.reverse();
            for (_, data) in &mut reversed {
                data.reverse();
            }
            let mut interner = Interner::new();
            let first = interner.intern(&borrowed(&groups));
            let second = interner.intern(&borrowed(&reversed));
            prop_assert!(Arc::ptr_eq(&first, &second));
        }
    }
}
