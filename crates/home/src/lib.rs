//! Runs the per-index write path (time checks, seq, fence, control, storage, fan-out),
//! crash-recovery and copy-mode opens, and companion writes.

use std::fmt;

use types::channel;

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
mod reader;
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
    /// Another writer holds control.
    Waiting,
    /// Control is held for the writer from before a restart, until it reopens or
    /// its grace ends.
    Reserved,
    /// The writer's control lease ran out. It stays out until it reopens.
    Expired,
    /// A stamp broke a rule.
    Order(order::Error),
    /// A series does not fit the group's count.
    Codec {
        /// The series' channel.
        channel: channel::Key,
        /// Why `codec` refused it.
        error: codec::Error,
    },
}

impl Refusal {
    /// The refusal of a write that the gate refused with `error`.
    ///
    /// # Panics
    ///
    /// For [`control::Error::Lease`], which no write gives.
    fn control(error: control::Error) -> Self {
        match error {
            control::Error::Waiting => Self::Waiting,
            control::Error::Reserved => Self::Reserved,
            control::Error::Expired => Self::Expired,
            control::Error::Lease { .. } => {
                panic!("invariant: a write makes no lease, got {error}")
            }
        }
    }
}

impl From<split::Error> for Refusal {
    fn from(error: split::Error) -> Self {
        let split::Error { channel, error } = error;
        Self::Codec { channel, error }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Waiting => control::Error::Waiting.fmt(f),
            Self::Reserved => control::Error::Reserved.fmt(f),
            Self::Expired => control::Error::Expired.fmt(f),
            Self::Order(error) => error.fmt(f),
            Self::Codec { channel, error } => write!(f, "channel {channel}: {error}"),
        }
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use types::channel::Slot;
    use types::time::Span;

    use super::*;
    use crate::common::key;

    #[test]
    fn names_the_channel_of_a_codec_refusal() {
        let refusal = Refusal::Codec {
            channel: key(Slot::new(2)),
            error: codec::Error::Length {
                expected: 12,
                actual: 8,
            },
        };

        assert_eq!(
            refusal.to_string(),
            "channel 02000000-0000-0000-0000-000000000002: the values hold 8 bytes, \
             but the samples take 12"
        );
    }

    #[test]
    fn says_why_the_gate_refused_a_write() {
        for (error, refusal, message) in [
            (
                control::Error::Waiting,
                Refusal::Waiting,
                "not in control: another writer holds the gate",
            ),
            (
                control::Error::Reserved,
                Refusal::Reserved,
                "not in control: held for the writer from before the restart",
            ),
            (
                control::Error::Expired,
                Refusal::Expired,
                "control lease ran out: reopen the writer to take control",
            ),
        ] {
            assert_eq!(Refusal::control(error), refusal);
            assert_eq!(refusal.to_string(), message);
        }
    }

    #[test]
    #[should_panic(expected = "invariant: a write makes no lease, got control lease \
                               must be longer than zero, got 0s")]
    fn panics_on_a_lease_error_from_a_write() {
        let _ = Refusal::control(control::Error::Lease { span: Span::ZERO });
    }
}
