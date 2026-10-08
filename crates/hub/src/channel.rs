//! The channels that sessions may name.

use types::channel;
use types::sample::Type;

/// What sessions read of a defined channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) key: channel::Key,
    /// The layout of its samples. Sessions read it only for a data channel.
    pub(crate) data_type: Type,
    /// The index it is on. An index names itself.
    pub(crate) index: channel::Key,
}
