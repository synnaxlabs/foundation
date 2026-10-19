//! Key sets: the channels of the frames of a session, with their types and index
//! groups.

use std::fmt;
use std::sync::Arc;

use crate::channel::{self, Slot, Slots};
use crate::hash;
use crate::sample::{Scalar, Type};

/// A key set's number in its node's [`Interner`]: dense from 0 and node-local, never
/// sent on the wire or stored on disk. Each set from [`KeySet::ordered`] has the same
/// key, one that no `Interner` gives, so the key does not tell two such sets apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u32);

impl Key {
    /// The key of each set from [`KeySet::ordered`].
    const ORDERED: Self = Self(u32::MAX);

    pub(super) const fn new(n: u32) -> Self {
        Self(n)
    }

    /// The key set number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The channels of the frames of a session, sorted by slot, with each entry's type and
/// index group. Every frame of the session points at it. An [`Interner`] makes the
/// key sets of a node, and [`KeySet::ordered`] makes the set of a stream that is of no
/// node.
#[derive(Debug)]
pub struct KeySet {
    key: Key,
    entries: Arc<[Entry]>,
    groups: Arc<[usize]>,
}

impl KeySet {
    /// The key set whose entries are `channels`, in order: entry `n` has slot `n`, and
    /// the third value of entry `n` is the entry of the index of its group. An index is
    /// the index of its own group. Group numbers follow the order of the index
    /// entries. Every such set has the same key, one that no [`Interner`] gives, so
    /// the key does not tell two of them apart. Its slots are its own, not a node's:
    /// never look up one in a [`Slots`].
    ///
    /// # Errors
    ///
    /// For the first entry in entry order that breaks a rule of the indexes,
    /// [`Error::Index`] or [`Error::Unstamped`]; else [`Error::Repeated`] with the
    /// least key that two entries share.
    ///
    /// # Panics
    ///
    /// When `channels` holds more than 2^32 entries.
    pub fn ordered(
        channels: impl IntoIterator<Item = (channel::Key, Type, usize)>,
    ) -> Result<Self, Error> {
        let channels: Vec<_> = channels.into_iter().collect();
        let stamp = Type::Scalar(Scalar::Stamp);
        for (at, &(_, data_type, index)) in channels.iter().enumerate() {
            if !matches!(channels.get(index), Some(&(.., own)) if own == index) {
                return Err(Error::Index { entry: at, index });
            }
            if at == index && data_type != stamp {
                return Err(Error::Unstamped {
                    entry: at,
                    sample: data_type,
                });
            }
        }
        let groups: Arc<[usize]> = (channels.iter().enumerate())
            .filter(|&(at, &(.., index))| at == index)
            .map(|(at, _)| at)
            .collect();
        let fit = "a key set holds at most 2^32 entries";
        let entry = |(at, &(key, data_type, index)): (usize, &(_, _, usize))| {
            let group = groups
                .binary_search(&index)
                .expect("invariant: the index of an entry is a group");
            Entry {
                slot: Slot::new(u32::try_from(at).expect(fit)),
                key,
                data_type,
                group: u32::try_from(group).expect(fit),
            }
        };
        let entries: Arc<[Entry]> = channels.iter().enumerate().map(entry).collect();
        if let Some(key) = repeated(entries.iter().map(|entry| entry.key)) {
            return Err(Error::Repeated(Repeated { key }));
        }
        Ok(Self {
            key: Key::ORDERED,
            entries,
            groups,
        })
    }

    /// The key set's number in its interner, or the one key of each set from
    /// [`KeySet::ordered`].
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
    /// The channel's node-local slot, or its position in a set from
    /// [`KeySet::ordered`].
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
    /// Each index gets its slot from [`Slots::index`], and each data channel from
    /// [`Slots::data`], in the order of `groups`, each index before its data. Groups
    /// are numbered in the order of their index slots. Equal groups, in any order, give
    /// the same key set. A new key set copies the snapshot's list, so it takes time
    /// linear in the number of key sets. Give it only groups that the node builds from
    /// the spec, never a subset of channels that a party outside the node picks: the
    /// table of key sets has no limit.
    ///
    /// # Panics
    ///
    /// If a channel appears twice: the `hub` builds groups from the spec, so that is a
    /// bug. Also if the slot table already assigned 2^32 slots, or the node already
    /// holds 2^32 - 1 key sets.
    pub fn intern(&mut self, groups: &[Group<'_>]) -> Arc<KeySet> {
        let stamp = Type::Scalar(Scalar::Stamp);
        let len = groups.iter().map(|group| group.data.len()).sum::<usize>();
        let mut channels = Vec::with_capacity(len.strict_add(groups.len()));
        let mut indexes = Vec::with_capacity(groups.len());
        for group in groups {
            let index = self.slots.index(group.index);
            indexes.push(index);
            channels.push((index, group.index, stamp, index));
            for &(key, data_type) in group.data {
                channels.push((self.slots.data(key, data_type), key, data_type, index));
            }
        }
        if let Some(key) = repeated(channels.iter().map(|&(_, key, ..)| key)) {
            panic!("{}", Repeated { key });
        }
        channels.sort_unstable_by_key(|&(slot, ..)| slot);
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
        let set = Arc::new(KeySet {
            key: interned(self.sets.len()),
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

/// The key of a new set of an [`Interner`] that holds `count` sets.
///
/// # Panics
///
/// When the key would be [`Key::ORDERED`] or past it.
fn interned(count: usize) -> Key {
    u32::try_from(count)
        .ok()
        .filter(|&n| n < Key::ORDERED.0)
        .map(Key::new)
        .expect("a node holds at most 2^32 - 1 key sets")
}

/// A key set would hold a channel twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeated {
    /// The channel.
    pub key: channel::Key,
}

impl fmt::Display for Repeated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "channel {} appears twice in a key set", self.key)
    }
}

impl std::error::Error for Repeated {}

/// Why [`KeySet::ordered`] refuses its channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The index of `entry` is entry `index`, which is not an entry whose index is
    /// itself.
    Index {
        /// The entry.
        entry: usize,
        /// The index that it names.
        index: usize,
    },
    /// Entry `entry` is an index of type `sample`, not `Stamp`.
    Unstamped {
        /// The entry.
        entry: usize,
        /// Its sample type.
        sample: Type,
    },
    /// Two entries share a key.
    Repeated(Repeated),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index { entry, index } => write!(
                f,
                "the index of entry {entry} is entry {index}, which is not an entry \
                 whose index is itself"
            ),
            Self::Unstamped { entry, sample } => write!(
                f,
                "the index entry {entry} has type {sample}, not {}",
                Type::Scalar(Scalar::Stamp)
            ),
            Self::Repeated(repeated) => fmt::Display::fmt(repeated, f),
        }
    }
}

impl std::error::Error for Error {}

/// The least key that `keys` holds twice.
fn repeated(keys: impl Iterator<Item = channel::Key>) -> Option<channel::Key> {
    let mut keys: Vec<_> = keys.collect();
    keys.sort_unstable();
    keys.array_windows()
        .find(|[a, b]| a == b)
        .map(|&[key, _]| key)
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

    /// An interner where key `n` has slot `n`, for each `n` below 10: as an index for
    /// each `n` in `indexes`, else as a data channel of the type that `types` gives
    /// it, or `F64`.
    fn interner(indexes: &[u32], types: &[(u32, Type)]) -> Interner {
        let mut interner = Interner::new();
        for n in 0..10 {
            if indexes.contains(&n) {
                interner.slots().index(key(n));
            } else {
                let data_type = types
                    .iter()
                    .find_map(|&(at, data_type)| (at == n).then_some(data_type));
                interner.slots().data(key(n), data_type.unwrap_or(F64));
            }
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
        let mut interner = interner(&[4, 9], &[(7, U8)]);
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
        let mut interner = interner(&[5], &[]);
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
        let mut interner = interner(&[1, 4], &[(3, U8)]);
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
        let mut interner = interner(&[1], &[(2, STAMP)]);
        let first = interner.intern(&[Group {
            index: key(1),
            data: &[(key(2), STAMP)],
        }]);
        let swapped = interner.intern(&[Group {
            index: key(2),
            data: &[(key(1), STAMP)],
        }]);
        let known = |n, k| Entry {
            slot: slot(n),
            key: key(k),
            data_type: STAMP,
            group: 0,
        };
        assert_eq!(first.entries(), [known(1, 1), known(2, 2)]);
        assert_eq!(swapped.entries(), [known(10, 2), known(11, 1)]);
        assert_eq!(swapped.groups(), [0]);
        assert_eq!(swapped.key().get(), 1);
    }

    #[test]
    fn tells_an_index_from_a_stamp_channel_beside_another_group() {
        let mut interner = interner(&[1, 5], &[(2, STAMP)]);
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
        assert_eq!(swapped.groups(), [0, 1]);
    }

    #[test]
    fn snapshots_hold_the_key_sets_interned_before_them() {
        let mut interner = interner(&[0, 1], &[]);
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
        let early = interner.slots().data(key(7), F64);
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
        assert_eq!(interner.slots().index(key(3)), slot(1));
    }

    #[test]
    #[should_panic(
        expected = "channel 00000000-0000-0000-0000-000000000002 appears twice in a key set"
    )]
    fn refuses_a_data_channel_twice() {
        interner(&[1], &[]).intern(&[Group {
            index: key(1),
            data: &[(key(2), F64), (key(2), U8)],
        }]);
    }

    #[test]
    #[should_panic(
        expected = "channel 00000000-0000-0000-0000-000000000001 appears twice in a key set"
    )]
    fn refuses_an_index_that_is_also_data() {
        interner(&[1, 3], &[]).intern(&[
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
        interner(&[6], &[]).intern(&[group, group]);
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
        for (at, entry) in entries.iter().enumerate() {
            let slot = if set.index(at) == at {
                slots.index(entry.key)
            } else {
                slots.data(entry.key, entry.data_type)
            };
            prop_assert_eq!(slot, entry.slot);
        }
        let total: usize = groups.iter().map(|(_, data)| 1 + data.len()).sum();
        prop_assert_eq!(entries.len(), total);
        prop_assert_eq!(set.groups().len(), groups.len());
        for (index, data) in groups {
            let at = set.find(slots.index(*index)).unwrap();
            let group = entries[at].group;
            let numbered = set.groups().iter().position(|&position| position == at);
            prop_assert_eq!(entries[at].data_type, STAMP);
            prop_assert_eq!(set.index(at), at);
            prop_assert_eq!(numbered.and_then(|g| u32::try_from(g).ok()), Some(group));
            for (key, kind) in data {
                let at_data = set.find(slots.data(*key, *kind)).unwrap();
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

    mod ordered {
        use super::*;

        /// The set of `channels` as one group, whose index is the entry at `index`.
        fn one(
            channels: impl IntoIterator<Item = (channel::Key, Type)>,
            index: usize,
        ) -> Result<KeySet, Error> {
            let channels = channels.into_iter();
            KeySet::ordered(channels.map(|(key, data_type)| (key, data_type, index)))
        }

        fn check(channels: &[(u32, Type)], index: usize, repeated: u32) {
            let channels = channels.iter().map(|&(n, data_type)| (key(n), data_type));
            let error = one(channels, index).expect_err("a repeat");
            assert_eq!(error, Error::Repeated(Repeated { key: key(repeated) }));
            assert_eq!(
                error.to_string(),
                format!("channel {} appears twice in a key set", key(repeated))
            );
        }

        #[test]
        fn refuses_the_least_repeated_key() {
            check(&[(1, STAMP), (1, STAMP)], 0, 1);
            check(&[(10, F64), (1, STAMP), (10, U8)], 1, 10);
            check(&[(7, F64), (3, STAMP), (7, F64), (3, F64)], 1, 3);
        }

        /// No key of an `Interner` is the key of an ordered set. It calls the private
        /// `interned`, since no test can intern 2^32 - 1 sets.
        #[test]
        fn has_a_key_that_no_interner_gives() {
            let set = one([(key(1), STAMP)], 0).unwrap();
            assert_eq!(set.key(), Key::new(u32::MAX));
            let last = usize::try_from(u32::MAX - 1).unwrap();
            assert_eq!(interned(last), Key::new(u32::MAX - 1));
            assert_ne!(Interner::new().intern(&[]).key(), set.key());
        }

        /// The private `interned` refuses the key of an ordered set. No caller can see
        /// this bound: it shows only after 2^32 - 1 sets, which no test can intern.
        #[test]
        #[should_panic(expected = "a node holds at most 2^32 - 1 key sets")]
        fn panics_on_the_interned_set_of_the_ordered_key() {
            let _key = interned(usize::try_from(u32::MAX).unwrap());
        }

        /// The error of `channels`, and its message.
        fn refusal(channels: &[(u32, Type, usize)]) -> (Error, String) {
            let channels = channels
                .iter()
                .map(|&(n, data_type, index)| (key(n), data_type, index));
            let error = KeySet::ordered(channels).expect_err("a refusal");
            (error, error.to_string())
        }

        fn index(entry: usize, index: usize) -> (Error, String) {
            let message = format!(
                "the index of entry {entry} is entry {index}, which is not an entry \
                 whose index is itself"
            );
            (Error::Index { entry, index }, message)
        }

        fn unstamped(entry: usize, sample: Type) -> (Error, String) {
            let message =
                format!("the index entry {entry} has type {sample}, not timestamp");
            (Error::Unstamped { entry, sample }, message)
        }

        #[test]
        fn refuses_an_index_past_the_entries() {
            assert_eq!(refusal(&[(1, STAMP, 2), (2, F64, 2)]), index(0, 2));
        }

        #[test]
        fn refuses_an_index_that_is_not_a_stamp() {
            assert_eq!(refusal(&[(1, STAMP, 1), (2, F64, 1)]), unstamped(1, F64));
        }

        /// Distinct keys of random types, and the position of one `Stamp` among them.
        fn one_group() -> impl Strategy<Value = (Vec<(channel::Key, Type)>, usize)> {
            let kind = prop_oneof![Just(F64), Just(U8), Just(STAMP)];
            prop::collection::btree_set(0_u32..1000, 1..40)
                .prop_map(|keys| keys.into_iter().collect::<Vec<_>>())
                .prop_shuffle()
                .prop_flat_map(move |keys| {
                    let kinds = prop::collection::vec(kind.clone(), keys.len());
                    (Just(keys.clone()), kinds, 0..keys.len())
                })
                .prop_map(|(keys, kinds, index)| {
                    let mut channels: Vec<_> =
                        keys.into_iter().map(key).zip(kinds).collect();
                    channels[index].1 = STAMP;
                    (channels, index)
                })
        }

        proptest! {
            #[test]
            fn keeps_the_order_of_its_channels((channels, index) in one_group()) {
                let set = one(channels.iter().copied(), index).unwrap();
                prop_assert_eq!(set.key().get(), u32::MAX);
                prop_assert_eq!(set.groups(), [index]);
                prop_assert_eq!(set.entries().len(), channels.len());
                for (at, (entry, &(key, data_type))) in
                    set.entries().iter().zip(&channels).enumerate()
                {
                    let n = u32::try_from(at).unwrap();
                    prop_assert_eq!(*entry, Entry { slot: slot(n), key, data_type, group: 0 });
                    prop_assert_eq!(set.find(slot(n)), Some(at));
                    prop_assert_eq!(set.index(at), index);
                }
                let past = u32::try_from(channels.len()).unwrap();
                prop_assert_eq!(set.find(slot(past)), None);
            }
        }

        #[test]
        fn numbers_groups_by_the_order_of_their_indexes() {
            let set = KeySet::ordered([
                (key(5), F64, 3),
                (key(6), STAMP, 1),
                (key(7), U8, 1),
                (key(8), STAMP, 3),
            ])
            .unwrap();
            assert_eq!(set.key().get(), u32::MAX);
            assert_eq!(set.groups(), [1, 3]);
            assert_eq!(
                set.entries(),
                [
                    entry_of(0, 5, F64, 1),
                    entry_of(1, 6, STAMP, 0),
                    entry_of(2, 7, U8, 0),
                    entry_of(3, 8, STAMP, 1),
                ]
            );
            let indexes: Vec<_> = (0..4).map(|at| set.index(at)).collect();
            assert_eq!(indexes, [3, 1, 1, 3]);
        }

        fn entry_of(n: u32, channel: u32, data_type: Type, group: u32) -> Entry {
            Entry {
                slot: slot(n),
                key: key(channel),
                data_type,
                group,
            }
        }

        #[test]
        fn refuses_the_least_repeated_key_of_two_groups() {
            let error = KeySet::ordered([
                (key(9), STAMP, 0),
                (key(4), STAMP, 1),
                (key(9), F64, 1),
                (key(4), F64, 0),
            ])
            .expect_err("a repeat");
            assert_eq!(error, Error::Repeated(Repeated { key: key(4) }));
        }

        #[test]
        fn refuses_an_index_of_one_entry_past_the_entries() {
            assert_eq!(refusal(&[(1, STAMP, 0), (2, F64, 2)]), index(1, 2));
        }

        #[test]
        fn refuses_an_index_of_another_group() {
            let channels = [(1, STAMP, 0), (2, F64, 0), (3, F64, 1)];
            assert_eq!(refusal(&channels), index(2, 1));
        }

        #[test]
        fn refuses_a_second_index_that_is_not_a_stamp() {
            assert_eq!(refusal(&[(1, STAMP, 0), (2, F64, 1)]), unstamped(1, F64));
        }

        /// The first entry that breaks a rule of the indexes gives the error, and a
        /// repeat comes only after the rules of the indexes.
        #[test]
        fn refuses_the_first_entry_that_breaks_a_rule() {
            let channels = [(1, F64, 0), (1, F64, 5), (2, F64, 1)];
            assert_eq!(refusal(&channels), unstamped(0, F64));
            let channels = [(1, STAMP, 0), (1, F64, 5), (2, U8, 2)];
            assert_eq!(refusal(&channels), index(1, 5));
            let channels = [(1, STAMP, 0), (1, F64, 0), (2, F64, 1)];
            assert_eq!(refusal(&channels), index(2, 1));
        }

        /// Distinct keys, each data channel on a random index among them.
        fn channels() -> impl Strategy<Value = Vec<(channel::Key, Type, usize)>> {
            prop::collection::btree_set(0_u32..1000, 1..40)
                .prop_map(|keys| keys.into_iter().collect::<Vec<_>>())
                .prop_shuffle()
                .prop_flat_map(|keys| {
                    let count = keys.len();
                    let indexes = prop::collection::vec(any::<bool>(), count);
                    let picks =
                        prop::collection::vec(any::<prop::sample::Index>(), count);
                    (Just(keys), indexes, picks)
                })
                .prop_map(|(keys, indexes, picks)| {
                    let mut own: Vec<_> =
                        (0..keys.len()).filter(|&at| indexes[at]).collect();
                    if own.is_empty() {
                        own.push(0);
                    }
                    keys.into_iter()
                        .enumerate()
                        .map(|(at, n)| match own.binary_search(&at) {
                            Ok(_) => (key(n), STAMP, at),
                            Err(_) => (key(n), F64, own[picks[at].index(own.len())]),
                        })
                        .collect()
                })
        }

        proptest! {
            #[test]
            fn keeps_the_order_and_index_of_its_channels(channels in channels()) {
                let set = KeySet::ordered(channels.iter().copied()).unwrap();
                let own: Vec<_> = channels
                    .iter()
                    .enumerate()
                    .filter(|&(at, &(.., index))| at == index)
                    .map(|(at, _)| at)
                    .collect();
                prop_assert_eq!(set.groups(), own.as_slice());
                for (at, (entry, &(key, data_type, index))) in
                    set.entries().iter().zip(&channels).enumerate()
                {
                    prop_assert_eq!(entry.slot, slot(u32::try_from(at).unwrap()));
                    prop_assert_eq!((entry.key, entry.data_type), (key, data_type));
                    prop_assert_eq!(set.index(at), index);
                }
            }
        }
    }
}
