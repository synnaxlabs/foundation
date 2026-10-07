//! Decides who holds control of an index: authority, ties, control leases, handoffs,
//! start state after failover.
//!
//! A [`Gate`] is the state machine for one index. It does no I/O and reads no clock:
//! the caller passes the home's monotonic time to each input. Before a frame that an
//! input accepted is stored, the caller records [`Gate::handoff`] in the index log,
//! then calls [`Gate::recorded`]. The control channel publishes each recorded
//! handoff.

#![deny(clippy::wildcard_enum_match_arm)]

mod gate;
pub mod lease;

use std::fmt;

use types::authority::Authority;
use types::name::Name;

pub use gate::{Gate, Key, Permit};
pub use lease::Lease;

/// A writer as the gate sees it. The holder's value is what the home records and
/// publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    /// The subject that opened the writer.
    pub subject: Name,
    /// The writer's authority, already capped by access.
    pub authority: Authority,
}

impl fmt::Display for Writer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at authority {}", self.subject, self.authority)
    }
}

/// A change of holder, borrowed from the [`Gate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handoff<'a> {
    /// The new holder, or `None` when the gate is now empty.
    pub to: Option<&'a Writer>,
}

/// Why the gate refused a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Another writer holds control.
    Waiting,
    /// The gate is held for the writer that held control before a restart, until it
    /// reopens or its grace ends.
    Reserved,
    /// The writer's control lease ran out. It stays out of the gate until it reopens.
    Expired,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Waiting => {
                f.write_str("not in control: another writer holds the gate")
            }
            Self::Reserved => f.write_str(
                "not in control: held for the writer from before the restart",
            ),
            Self::Expired => {
                f.write_str("control lease ran out: reopen the writer to take control")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_to_do() {
        assert_eq!(
            Error::Waiting.to_string(),
            "not in control: another writer holds the gate"
        );
        assert_eq!(
            Error::Expired.to_string(),
            "control lease ran out: reopen the writer to take control"
        );
        assert_eq!(
            Error::Reserved.to_string(),
            "not in control: held for the writer from before the restart"
        );
    }

    #[test]
    fn writer_shows_subject_and_authority() {
        let writer = Writer {
            subject: "people.alice".parse().expect("valid name"),
            authority: Authority(200),
        };
        assert_eq!(writer.to_string(), "people.alice at authority 200");
    }
}
