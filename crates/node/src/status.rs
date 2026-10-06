//! The node's status channels and the collector that fills them from each crate.

#[cfg(test)]
#[cfg(not(loom))]
mod tests;

use clock::Status;
use estimate::Measurement;
use types::sample::{Scalar, Type};
use types::time::Span;

/// One status channel, under the node's name.
#[derive(Debug)]
pub(crate) struct Channel {
    /// The name after `<node>.`.
    pub(crate) name: &'static str,
    pub(crate) data_type: Type,
    /// The channel's value from the pulled status, or `None` for no sample.
    read: fn(Status) -> Option<Value>,
}

/// The node's status channels, a fixed set per release. [`Collector::collect`] gives
/// a value for each, in this order.
pub(crate) const TABLE: [Channel; 3] = [
    Channel {
        name: "clock.status",
        data_type: Type::Scalar(Scalar::U8),
        read: |status| {
            let code = match status {
                Status::Unsynced(_) => 0,
                Status::Synced(_) => 1,
                Status::Holdover(..) => 2,
            };
            Some(Value::U8(code))
        },
    },
    // Mesh time minus monotonic time.
    Channel {
        name: "clock.offset",
        data_type: Type::Scalar(Scalar::Span),
        read: |status| time(status).map(|m| Value::Span(m.offset())),
    },
    // An unknown error is the bound that `estimate::Measurement::unknown` gives.
    Channel {
        name: "clock.error",
        data_type: Type::Scalar(Scalar::Span),
        read: |status| time(status).map(|m| Value::Span(m.error())),
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
    /// channel with no sample now. Reads each crate once. An unsynced clock has no
    /// offset or error.
    pub(crate) fn collect(&self) -> [Option<Value>; TABLE.len()] {
        let status = self.clock.status();
        TABLE.each_ref().map(|channel| (channel.read)(status))
    }
}

fn time(status: Status) -> Option<Measurement> {
    match status {
        Status::Unsynced(_) => None,
        Status::Synced(m) | Status::Holdover(m, _) => Some(m),
    }
}
