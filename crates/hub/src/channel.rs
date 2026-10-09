//! The channels that sessions may name.

use spec::channel::Kind;
use types::channel;
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
