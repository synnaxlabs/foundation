//! Runs the per-index write path (time checks, seq, fence, control, storage, fan-out),
//! crash-recovery and copy-mode opens, and companion writes.

use std::fmt;

#[cfg(test)]
mod common;
#[cfg_attr(not(test), expect(dead_code, reason = "the shard is the first user"))]
mod handoff;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod index;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod order;
#[cfg_attr(not(test), expect(dead_code, reason = "node is the first user"))]
mod shard;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod split;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod stored;
#[cfg_attr(not(test), expect(dead_code, reason = "node is the first user"))]
mod writer;

/// Why the home refused a group of a writer's frame.
#[cfg_attr(not(test), expect(dead_code, reason = "node is the first user"))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum Refusal {
    /// The gate refused the write.
    Control(control::Error),
    /// A stamp broke a rule.
    Order(order::Error),
    /// A series did not fit the group's count.
    Codec(split::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Order(error) => error.fmt(f),
            Self::Codec(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use types::channel::Slot;

    use super::*;
    use crate::common::key;

    #[test]
    fn names_the_channel_of_a_codec_refusal() {
        let refusal = Refusal::Codec(split::Error {
            channel: key(Slot::new(2)),
            error: codec::Error::Length {
                expected: 12,
                actual: 8,
            },
        });

        assert_eq!(
            refusal.to_string(),
            "channel 02000000-0000-0000-0000-000000000002: the values hold 8 bytes, \
             but the samples take 12"
        );
    }
}
