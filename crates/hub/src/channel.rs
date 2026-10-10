//! The channels that sessions may name.

use spec::channel::Kind;
use types::channel;
use types::hash;
use types::name::Name;
use types::sample::{Scalar, Type};

/// A defined channel, which sessions read at their open.
#[derive(Clone, Debug)]
pub(crate) struct Channel(pub(crate) spec::channel::Channel);

impl Channel {
    pub(crate) fn key(&self) -> channel::Key {
        self.0.key
    }

    /// The index it is on. An index names itself.
    pub(crate) fn index(&self) -> channel::Key {
        match &self.0.kind {
            Kind::Index { .. } => self.0.key,
            Kind::Data(data) => *data.index(),
        }
    }

    /// The layout of its samples.
    pub(crate) fn sample(&self) -> Type {
        match &self.0.kind {
            Kind::Index { .. } => Type::Scalar(Scalar::Stamp),
            Kind::Data(data) => data.data_type().sample(),
        }
    }
}

/// The defined channels, by name and by key.
#[derive(Debug, Default)]
pub(crate) struct Table {
    /// The key of each channel in `defined`, by name.
    names: hash::Map<Name, channel::Key>,
    defined: hash::Map<channel::Key, Channel>,
}

/// What a [`Table::set`] removed.
pub(crate) struct Removed {
    /// Each channel that is removed or changed.
    pub(crate) channels: hash::Set<channel::Key>,
    /// Each index that is no longer defined, in key order.
    pub(crate) indexes: Vec<channel::Key>,
}

impl Table {
    /// Makes `channels` the defined channels. A channel whose name or definition
    /// changed is removed, then defined again.
    pub(crate) fn set(
        &mut self,
        channels: &hash::Map<&Name, &spec::channel::Channel>,
    ) -> Removed {
        let removed: hash::Set<channel::Key> = self
            .names
            .iter()
            .filter(|&(name, &key)| channels.get(name) != Some(&&self.known(key).0))
            .map(|(_, &key)| key)
            .collect();
        let index = |channel: &spec::channel::Channel| {
            matches!(channel.kind, Kind::Index { .. }).then_some(channel.key)
        };
        let after: hash::Set<channel::Key> = channels
            .values()
            .filter_map(|channel| index(channel))
            .collect();
        let mut indexes: Vec<channel::Key> = self
            .defined
            .values()
            .filter_map(|known| index(&known.0))
            .filter(|key| !after.contains(key))
            .collect();
        indexes.sort_unstable();
        self.names.retain(|_, key| !removed.contains(key));
        self.defined.retain(|key, _| !removed.contains(key));
        let mut new: Vec<_> = channels
            .iter()
            .filter(|&(name, _)| !self.names.contains_key(*name))
            .collect();
        new.sort_unstable_by_key(|&(name, _)| *name);
        for (&name, &channel) in new {
            self.names.insert(name.clone(), channel.key);
            self.defined.insert(channel.key, Channel(channel.clone()));
        }
        Removed {
            channels: removed,
            indexes,
        }
    }

    /// The channel named `name`, if any.
    pub(crate) fn named(&self, name: &Name) -> Option<&Channel> {
        let &key = self.names.get(name)?;
        Some(self.known(key))
    }

    /// Each defined channel and its name, in no set order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Name, &Channel)> {
        self.names
            .iter()
            .map(|(name, &key)| (name, self.known(key)))
    }

    /// The channel `key`, if it is defined.
    pub(crate) fn get(&self, key: channel::Key) -> Option<&Channel> {
        self.defined.get(&key)
    }

    /// The channel `key`.
    ///
    /// # Panics
    ///
    /// When `key` is not defined: each caller holds a defined key.
    pub(crate) fn known(&self, key: channel::Key) -> &Channel {
        let channel = self.defined.get(&key);
        channel.unwrap_or_else(|| panic!("invariant: channel {key} is defined"))
    }
}
