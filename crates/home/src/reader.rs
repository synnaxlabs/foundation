//! A reader session on one index of a shard.

use types::channel::Slot;

/// An open reader on its shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Key {
    /// The slot of the reader's index.
    pub(crate) slot: Slot,
    /// The reader's session on the index.
    pub(crate) session: delivery::Key,
}

/// Which frames a reader gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Each live frame from the index's live tail on, after the commit that holds it,
    /// while the reader has credit for it.
    Complete {
        /// The reader's credit since it opened, in bytes.
        limit_bytes: u64,
    },
    /// The newest live frame, before its commit.
    Latest,
}
