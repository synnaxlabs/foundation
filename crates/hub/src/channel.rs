//! The channels that sessions may name.

use types::channel;
use types::sample::Type;

/// What sessions read of a defined channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) key: channel::Key,
    /// The layout of its samples. An index has `Stamp`.
    pub(crate) data_type: Type,
    /// The index it is on. An index names itself.
    pub(crate) index: channel::Key,
}
