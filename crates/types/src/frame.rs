//! Frames, the key sets they point at, and the node's table of slots and key sets.

pub mod key_set;

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::channel;
use crate::hash;
use crate::sample::{Scalar, Type};
use key_set::{Entry, Group, KeySet, Snapshot};

/// The node's table of channel slots and key sets. `node` makes one and injects it into
/// `hub` and `home`, which intern at session open. Methods take `&mut self`: the caller
/// serializes them.
#[derive(Debug, Default)]
pub struct Interner {
    slots: hash::Map<channel::Key, channel::Slot>,
    sets: hash::Set<Interned>,
    snapshot: Snapshot,
}

impl Interner {
    /// An interner with no slots and no key sets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot of `key`. The first call for a key assigns the next slot, from 0. A
    /// slot is never reused.
    ///
    /// # Panics
    ///
    /// If the node already holds 2^32 channels.
    pub fn slot(&mut self, key: channel::Key) -> channel::Slot {
        let next = self.slots.len();
        *self.slots.entry(key).or_insert_with(|| {
            let n = u32::try_from(next).expect("a node holds at most 2^32 channels");
            channel::Slot::new(n)
        })
    }

    /// The key set of `groups`: each index, with type `Stamp`, and each data channel.
    /// Groups are numbered in the order of their index slots. Equal groups, in any
    /// order, give the same key set. A new key set copies the snapshot's list, so it
    /// takes time linear in the number of key sets.
    ///
    /// # Panics
    ///
    /// If a slot appears twice, or there are more than 65,536 groups: the `hub` builds
    /// groups from the spec, so either is a bug. Also if the node already holds 2^32
    /// key sets.
    pub fn intern(&mut self, groups: &[Group<'_>]) -> Arc<KeySet> {
        let mut indexes: Vec<channel::Slot> = groups.iter().map(|g| g.index).collect();
        indexes.sort_unstable();
        let number = |index| {
            let at = indexes.partition_point(|&other| other < index);
            u16::try_from(at).expect("a key set holds at most 65,536 groups")
        };
        let mut entries: Vec<Entry> = groups
            .iter()
            .flat_map(|g| {
                let group = number(g.index);
                let index = (g.index, Type::Scalar(Scalar::Stamp));
                [index]
                    .into_iter()
                    .chain(g.data.iter().copied())
                    .map(move |(slot, kind)| Entry { slot, kind, group })
            })
            .collect();
        entries.sort_unstable_by_key(|entry| entry.slot);
        if let Some([entry, _]) =
            entries.array_windows().find(|[a, b]| a.slot == b.slot)
        {
            panic!("slot {} appears twice in a key set", entry.slot.get());
        }
        if let Some(Interned(set)) = self.sets.get(entries.as_slice()) {
            return Arc::clone(set);
        }
        let positions = indexes
            .iter()
            .map(|&index| {
                entries
                    .binary_search_by_key(&index, |entry| entry.slot)
                    .expect("invariant: each index is an entry")
            })
            .collect();
        let n =
            u32::try_from(self.sets.len()).expect("a node holds at most 2^32 key sets");
        let set = Arc::new(KeySet::new(
            key_set::Key::new(n),
            entries.into_boxed_slice(),
            positions,
        ));
        self.snapshot = Snapshot(
            self.snapshot
                .0
                .iter()
                .cloned()
                .chain([Arc::clone(&set)])
                .collect(),
        );
        self.sets.insert(Interned(Arc::clone(&set)));
        set
    }

    /// The key sets interned so far.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }
}

/// A key set that hashes and compares by its entries, so the interner finds it from a
/// slice of entries.
#[derive(Debug)]
struct Interned(Arc<KeySet>);

impl Borrow<[Entry]> for Interned {
    fn borrow(&self) -> &[Entry] {
        self.0.entries()
    }
}

impl PartialEq for Interned {
    fn eq(&self, other: &Self) -> bool {
        self.0.entries() == other.0.entries()
    }
}

impl Eq for Interned {}

impl Hash for Interned {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.entries().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Slot;
    use proptest::prelude::*;

    const STAMP: Type = Type::Scalar(Scalar::Stamp);
    const F64: Type = Type::Scalar(Scalar::F64);
    const U8: Type = Type::Scalar(Scalar::U8);

    fn slot(n: u32) -> Slot {
        Slot::new(n)
    }

    #[test]
    fn assigns_dense_slots_once_per_key() {
        let mut interner = Interner::new();
        let a = channel::Key::from_u128(7);
        let b = channel::Key::from_u128(3);
        assert_eq!(interner.slot(a), slot(0));
        assert_eq!(interner.slot(b), slot(1));
        assert_eq!(interner.slot(a), slot(0));
        assert_eq!(interner.slot(channel::Key::from_u128(9)), slot(2));
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
        let entry = |n, kind, group| Entry {
            slot: slot(n),
            kind,
            group,
        };
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
    #[should_panic(expected = "a key set holds at most 65,536 groups")]
    fn refuses_more_than_65536_groups() {
        let groups: Vec<Group<'_>> = (0..=65_536)
            .map(|n| Group {
                index: slot(n),
                data: &[],
            })
            .collect();
        Interner::new().intern(&groups);
    }

    #[test]
    fn holds_65536_groups() {
        let groups: Vec<Group<'_>> = (0..65_536)
            .map(|n| Group {
                index: slot(n),
                data: &[],
            })
            .collect();
        let set = Interner::new().intern(&groups);
        assert_eq!(set.groups().len(), 65_536);
        assert_eq!(set.entries().last().map(|e| e.group), Some(u16::MAX));
    }

    type Groups = Vec<(Slot, Vec<(Slot, Type)>)>;

    /// Up to 5 groups over distinct slots, with data channels of random types.
    fn groups() -> impl Strategy<Value = Groups> {
        prop::collection::btree_set(0_u32..1000, 1..40)
            .prop_map(|slots| slots.into_iter().collect::<Vec<_>>())
            .prop_shuffle()
            .prop_flat_map(|slots| {
                let n = slots.len();
                let kind = prop_oneof![Just(F64), Just(U8)];
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

    fn borrowed(groups: &Groups) -> Vec<Group<'_>> {
        groups
            .iter()
            .map(|(index, data)| Group {
                index: *index,
                data,
            })
            .collect()
    }

    proptest! {
        #[test]
        fn holds_each_channel_once_under_its_index(groups in groups()) {
            let set = Interner::new().intern(&borrowed(&groups));
            let entries = set.entries();
            prop_assert!(entries.is_sorted_by(|a, b| a.slot < b.slot));
            let total: usize = groups.iter().map(|(_, data)| 1 + data.len()).sum();
            prop_assert_eq!(entries.len(), total);
            for (index, data) in &groups {
                let at = set.find(*index).unwrap();
                let group = usize::from(entries[at].group);
                prop_assert_eq!(entries[at].kind, STAMP);
                prop_assert_eq!(set.groups()[group], at);
                for (slot, kind) in data {
                    let entry = entries[set.find(*slot).unwrap()];
                    prop_assert_eq!(entry.kind, *kind);
                    prop_assert_eq!(usize::from(entry.group), group);
                }
            }
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
