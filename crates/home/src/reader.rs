//! A reader session on one index of a shard.

use types::channel::Slot;

/// A reader on its shard: the slot of its index and its session there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Key {
    /// The slot of the reader's index.
    pub(crate) slot: Slot,
    /// The reader's session on the index.
    pub(crate) session: delivery::Key,
}
