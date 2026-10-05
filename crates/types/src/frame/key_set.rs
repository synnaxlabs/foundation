//! Key sets: the channels that a writer session sends, interned once per session.

use std::sync::Arc;

use crate::channel::{self, Slot, Slots};
use crate::hash;
use crate::sample::{Scalar, Type};

/// A key set's number in its node's [`Interner`]: dense from 0 and node-local, never
/// sent on the wire or stored on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u32);

impl Key {
    pub(super) const fn new(n: u32) -> Self {
        Self(n)
    }

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

    /// The position in [`Self::entries`] of the index of `entry`'s group.
    ///
    /// # Panics
    ///
    /// If `entry` is out of range.
    #[must_use]
    pub fn index(&self, entry: usize) -> usize {
        self.groups[super::to_usize(self.entries[entry].group)]
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
    /// The channel's node-local slot.
    pub slot: Slot,
    /// The channel.
    pub key: channel::Key,
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
    pub index: channel::Key,
    /// Each data channel on the index, with the layout of its samples.
    pub data: &'a [(channel::Key, Type)],
}

/// The node's table of key sets, which owns the node's slot table. `node` makes one and
/// injects it into `hub` and `home`, which intern at session open. Methods take
/// `&mut self`: the caller serializes them.
#[derive(Debug, Default)]
pub struct Interner {
    slots: Slots,
    sets: hash::Map<Shape, Arc<KeySet>>,
    snapshot: Snapshot,
}

impl Interner {
    /// An interner with no key sets and no slots.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The node's slot table, for a caller that needs a slot outside a key set, such as
    /// the index of a recovered buffer.
    pub fn slots(&mut self) -> &mut Slots {
        &mut self.slots
    }

    /// The key set of `groups`: each index, with type `Stamp`, and each data channel.
    /// Each channel gets its slot from [`Self::slots`], which assigns one to each new
    /// key in the order of `groups`, each index before its data. Groups are numbered
    /// in the order of their index slots. Equal groups, in any order, give the same key
    /// set. A new key set copies the snapshot's list, so it takes time linear in the
    /// number of key sets.
    ///
    /// # Panics
    ///
    /// If a channel appears twice: the `hub` builds groups from the spec, so that is a
    /// bug. Also if the slot table already holds 2^32 channels, or the node already
    /// holds 2^32 key sets.
    pub fn intern(&mut self, groups: &[Group<'_>]) -> Arc<KeySet> {
        let stamp = Type::Scalar(Scalar::Stamp);
        let len = groups.iter().map(|group| group.data.len()).sum::<usize>();
        let mut channels = Vec::with_capacity(len.strict_add(groups.len()));
        let mut indexes = Vec::with_capacity(groups.len());
        for group in groups {
            let index = self.slots.assign(group.index);
            indexes.push(index);
            channels.push((index, group.index, stamp, index));
            for &(key, data_type) in group.data {
                channels.push((self.slots.assign(key), key, data_type, index));
            }
        }
        channels.sort_unstable_by_key(|&(slot, ..)| slot);
        if let Some([(_, key, ..), _]) =
            channels.array_windows().find(|[a, b]| a.0 == b.0)
        {
            panic!("channel {key} appears twice in a key set");
        }
        indexes.sort_unstable();
        let entries: Arc<[Entry]> = channels
            .iter()
            .map(|&(slot, key, data_type, index)| {
                let at = indexes.partition_point(|&other| other < index);
                let group =
                    u32::try_from(at).expect("invariant: index slots are distinct");
                Entry {
                    slot,
                    key,
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
            key: Key::new(n),
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

    fn key(n: u32) -> channel::Key {
        channel::Key::from_u128(u128::from(n))
    }

    /// An interner where key `n` has slot `n`, for each `n` below `len`.
    fn interner(len: u32) -> Interner {
        let mut interner = Interner::new();
        for n in 0..len {
            interner.slots().assign(key(n));
        }
        interner
    }

    fn entry(n: u32, data_type: Type, group: u32) -> Entry {
        Entry {
            slot: slot(n),
            key: key(n),
            data_type,
            group,
        }
    }

    #[test]
    fn sorts_entries_and_numbers_groups_by_index_slot() {
        let mut interner = interner(10);
        let set = interner.intern(&[
            Group {
                index: key(9),
                data: &[(key(2), F64)],
            },
            Group {
                index: key(4),
                data: &[(key(7), U8), (key(1), F64)],
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
        assert_eq!(
            (0..5).map(|e| set.index(e)).collect::<Vec<_>>(),
            [2, 4, 2, 2, 4]
        );
        assert_eq!(set.key().get(), 0);
    }

    #[test]
    fn finds_the_entry_of_each_slot() {
        let mut interner = interner(10);
        let set = interner.intern(&[Group {
            index: key(5),
            data: &[(key(8), F64), (key(2), F64)],
        }]);
        assert_eq!(set.find(slot(2)), Some(0));
        assert_eq!(set.find(slot(5)), Some(1));
        assert_eq!(set.find(slot(8)), Some(2));
        assert_eq!(set.find(slot(3)), None);
    }

    #[test]
    fn gives_equal_groups_the_same_key_set() {
        let mut interner = interner(10);
        let first = interner.intern(&[
            Group {
                index: key(1),
                data: &[(key(2), F64), (key(3), U8)],
            },
            Group {
                index: key(4),
                data: &[],
            },
        ]);
        let reordered = interner.intern(&[
            Group {
                index: key(4),
                data: &[],
            },
            Group {
                index: key(1),
                data: &[(key(3), U8), (key(2), F64)],
            },
        ]);
        let other = interner.intern(&[Group {
            index: key(1),
            data: &[(key(2), U8), (key(3), U8)],
        }]);
        assert!(Arc::ptr_eq(&first, &reordered));
        assert_eq!(first.key().get(), 0);
        assert_eq!(other.key().get(), 1);
    }

    #[test]
    fn tells_an_index_from_a_stamp_channel_on_it() {
        let mut interner = interner(10);
        let first = interner.intern(&[Group {
            index: key(1),
            data: &[(key(2), STAMP)],
        }]);
        let swapped = interner.intern(&[Group {
            index: key(2),
            data: &[(key(1), STAMP)],
        }]);
        assert_eq!(first.entries(), swapped.entries());
        assert_eq!(first.groups(), [0]);
        assert_eq!(swapped.groups(), [1]);
        assert_eq!(swapped.key().get(), 1);
    }

    #[test]
    fn tells_an_index_from_a_stamp_channel_beside_another_group() {
        let mut interner = interner(10);
        let first = interner.intern(&[
            Group {
                index: key(1),
                data: &[(key(2), STAMP)],
            },
            Group {
                index: key(5),
                data: &[],
            },
        ]);
        let swapped = interner.intern(&[
            Group {
                index: key(2),
                data: &[(key(1), STAMP)],
            },
            Group {
                index: key(5),
                data: &[],
            },
        ]);
        assert_eq!(first.groups(), [0, 2]);
        assert_eq!(swapped.groups(), [1, 2]);
    }

    #[test]
    fn snapshots_hold_the_key_sets_interned_before_them() {
        let mut interner = interner(10);
        let empty = interner.snapshot();
        let first = interner.intern(&[Group {
            index: key(0),
            data: &[],
        }]);
        let one = interner.snapshot();
        let second = interner.intern(&[Group {
            index: key(1),
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
    fn assigns_slots_to_new_keys_in_group_order() {
        let mut interner = Interner::new();
        let first = interner.intern(&[
            Group {
                index: key(50),
                data: &[(key(45), F64), (key(40), U8)],
            },
            Group {
                index: key(30),
                data: &[(key(20), U8)],
            },
        ]);
        let known = |n, k, data_type, group| Entry {
            slot: slot(n),
            key: key(k),
            data_type,
            group,
        };
        assert_eq!(
            first.entries(),
            [
                known(0, 50, STAMP, 0),
                known(1, 45, F64, 0),
                known(2, 40, U8, 0),
                known(3, 30, STAMP, 1),
                known(4, 20, U8, 1),
            ]
        );
        let second = interner.intern(&[Group {
            index: key(30),
            data: &[(key(10), F64)],
        }]);
        assert_eq!(
            second.entries(),
            [known(3, 30, STAMP, 0), known(5, 10, F64, 0)]
        );
    }

    #[test]
    fn keeps_a_slot_assigned_before_the_key_set() {
        let mut interner = Interner::new();
        let early = interner.slots().assign(key(7));
        let set = interner.intern(&[Group {
            index: key(3),
            data: &[(key(7), F64)],
        }]);
        let known = |n, k, data_type| Entry {
            slot: slot(n),
            key: key(k),
            data_type,
            group: 0,
        };
        assert_eq!(early, slot(0));
        assert_eq!(set.entries(), [known(0, 7, F64), known(1, 3, STAMP)]);
        assert_eq!(interner.slots().assign(key(3)), slot(1));
    }

    #[test]
    #[should_panic(
        expected = "channel 00000000-0000-0000-0000-000000000002 appears twice in a key set"
    )]
    fn refuses_a_data_channel_twice() {
        interner(10).intern(&[Group {
            index: key(1),
            data: &[(key(2), F64), (key(2), U8)],
        }]);
    }

    #[test]
    #[should_panic(
        expected = "channel 00000000-0000-0000-0000-000000000001 appears twice in a key set"
    )]
    fn refuses_an_index_that_is_also_data() {
        interner(10).intern(&[
            Group {
                index: key(1),
                data: &[],
            },
            Group {
                index: key(3),
                data: &[(key(1), STAMP)],
            },
        ]);
    }

    #[test]
    #[should_panic(
        expected = "channel 00000000-0000-0000-0000-000000000006 appears twice in a key set"
    )]
    fn refuses_an_index_twice() {
        let group = Group {
            index: key(6),
            data: &[],
        };
        interner(10).intern(&[group, group]);
    }

    #[test]
    fn holds_more_groups_than_a_u16_counts() {
        let groups: Vec<Group<'_>> = (0..70_000)
            .map(|n| Group {
                index: key(n),
                data: &[],
            })
            .collect();
        let set = Interner::new().intern(&groups);
        assert_eq!(set.groups().len(), 70_000);
        assert_eq!(set.entries().last(), Some(&entry(69_999, STAMP, 69_999)));
    }

    type Groups = Vec<(channel::Key, Vec<(channel::Key, Type)>)>;

    /// Up to 5 groups over distinct keys, with data channels of random types.
    fn groups() -> impl Strategy<Value = Groups> {
        prop::collection::btree_set(0_u32..1000, 1..40)
            .prop_map(|keys| keys.into_iter().collect::<Vec<_>>())
            .prop_shuffle()
            .prop_flat_map(|keys| {
                let n = keys.len();
                let kind = prop_oneof![Just(F64), Just(U8), Just(STAMP)];
                let picks = prop::collection::vec((0_usize..5, kind), n);
                (Just(keys), 1..=n.min(5), picks)
            })
            .prop_map(|(keys, count, picks)| {
                let (indexes, data) = keys.split_at(count);
                let mut groups: Groups =
                    indexes.iter().map(|&n| (key(n), Vec::new())).collect();
                for (&n, &(group, kind)) in data.iter().zip(&picks) {
                    groups[group % count].1.push((key(n), kind));
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

    /// Checks that `set` holds each channel of `groups` once, under its own index,
    /// with the slot that `slots` gives its key.
    fn check(
        set: &KeySet,
        groups: &Groups,
        slots: &mut Slots,
    ) -> Result<(), TestCaseError> {
        let entries = set.entries();
        prop_assert!(entries.is_sorted_by(|a, b| a.slot < b.slot));
        for entry in entries {
            prop_assert_eq!(slots.assign(entry.key), entry.slot);
        }
        let total: usize = groups.iter().map(|(_, data)| 1 + data.len()).sum();
        prop_assert_eq!(entries.len(), total);
        prop_assert_eq!(set.groups().len(), groups.len());
        for (index, data) in groups {
            let at = set.find(slots.assign(*index)).unwrap();
            let group = entries[at].group;
            let numbered = set.groups().iter().position(|&position| position == at);
            prop_assert_eq!(entries[at].data_type, STAMP);
            prop_assert_eq!(set.index(at), at);
            prop_assert_eq!(numbered.and_then(|g| u32::try_from(g).ok()), Some(group));
            for (key, kind) in data {
                let at_data = set.find(slots.assign(*key)).unwrap();
                prop_assert_eq!(entries[at_data].data_type, *kind);
                prop_assert_eq!(entries[at_data].group, group);
                prop_assert_eq!(set.index(at_data), at);
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
            check(&first, &groups, interner.slots())?;
            check(&second, &other, interner.slots())?;
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
