//! What a writer opens with on the indexes of one shard, and the key of an open
//! writer.

use std::sync::Arc;

use types::frame::key_set::KeySet;

/// What a writer opens with: its control, its lease, and the key set of its frames.
#[derive(Clone, Debug)]
pub(crate) struct Writer {
    /// The subject and its authority, already capped by access.
    pub(crate) control: control::Writer,
    /// The control lease, if the writer set one.
    pub(crate) lease: Option<control::Lease>,
    /// The key set of every frame the writer writes.
    pub(crate) set: Arc<KeySet>,
}

/// An open writer on its shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key(pub(crate) u64);
