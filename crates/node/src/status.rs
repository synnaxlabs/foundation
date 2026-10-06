//! The node's status channels and the collector that fills them from each crate.

#[cfg(test)]
mod tests;

use clock::Status;
use types::sample::{Scalar, Type};
use types::time::Span;

/// One status channel, under the node's name.
#[derive(Debug)]
pub(crate) struct Channel {
    /// The name after `<node>.`.
    pub(crate) name: &'static str,
    pub(crate) kind: Type,
}

/// The node's status channels, a fixed set per release. [`Collector::collect`] gives
/// a value for each, in this order.
pub(crate) const TABLE: [Channel; 3] = [
    // 0 unsynced, 1 synced, 2 holdover.
    Channel {
        name: "clock.status",
        kind: Type::Scalar(Scalar::U8),
    },
    // Mesh time minus monotonic time.
    Channel {
        name: "clock.offset",
        kind: Type::Scalar(Scalar::Span),
    },
    // The bound on the offset's error. 36500 days means unknown.
    Channel {
        name: "clock.error",
        kind: Type::Scalar(Scalar::Span),
    },
];

/// One sample of a status channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    U8(u8),
    Span(Span),
}

/// Pulls the status of each crate from a reader that the crate gives.
#[derive(Debug)]
pub(crate) struct Collector {
    clock: clock::Reader,
}

impl Collector {
    pub(crate) fn new(clock: clock::Reader) -> Self {
        Self { clock }
    }

    /// The value of each channel in [`TABLE`] now, in its order, or `None` for a
    /// channel with no sample now. An unsynced clock has no offset or error.
    pub(crate) fn collect(&self) -> [Option<Value>; TABLE.len()] {
        let (status, time) = match self.clock.status() {
            Status::Unsynced(_) => (0, None),
            Status::Synced(m) => (1, Some(m)),
            Status::Holdover(m, _) => (2, Some(m)),
        };
        [
            Some(Value::U8(status)),
            time.map(|m| Value::Span(m.offset())),
            time.map(|m| Value::Span(m.error())),
        ]
    }
}
