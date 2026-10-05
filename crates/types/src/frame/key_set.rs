//! Key sets: the channels that a writer session sends, interned once per session.

use std::sync::Arc;

use crate::channel::Slot;
use crate::sample::Type;

/// A key set's number in its node's [`Interner`](super::Interner): dense from 0 and
/// node-local, never sent on the wire or stored on disk.
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
/// index group. Every frame of the session points at it. Only an
/// [`Interner`](super::Interner) makes one.
#[derive(Debug)]
pub struct KeySet {
    key: Key,
    entries: Box<[Entry]>,
    groups: Box<[usize]>,
}

impl KeySet {
    pub(super) const fn new(
        key: Key,
        entries: Box<[Entry]>,
        groups: Box<[usize]>,
    ) -> Self {
        Self {
            key,
            entries,
            groups,
        }
    }

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
    pub kind: Type,
    /// The entry's index group: a position in [`KeySet::groups`].
    pub group: u16,
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

/// The key sets of an [`Interner`](super::Interner) when it was taken. Cloning it is
/// cheap, so each shard keeps one and reads it without the interner.
#[derive(Clone, Debug, Default)]
pub struct Snapshot(pub(super) Arc<[Arc<KeySet>]>);

impl Snapshot {
    /// The key set with `key`, or `None` when it was interned after this snapshot.
    #[must_use]
    pub fn get(&self, key: Key) -> Option<&Arc<KeySet>> {
        self.0.get(usize::try_from(key.get()).ok()?)
    }
}
