//! The channels that sessions may name.

use types::channel;
use types::name::Name;
use types::sample::Type;

/// A channel that sessions may name. [`Hub::define`](crate::Hub::define) takes it
/// until the hub reads channels from the spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// The channel's key.
    pub key: channel::Key,
    /// The name that sessions use.
    pub name: Name,
    /// The layout of its samples. An index has `Stamp`.
    pub data_type: Type,
    /// The index it is on. An index names itself.
    pub index: channel::Key,
}
