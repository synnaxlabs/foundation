//! A writer session on the indexes of one shard.

use std::sync::Arc;

use types::frame::key_set::KeySet;

/// A writer session on the indexes of one shard.
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
