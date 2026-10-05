//! Decides who holds control of an index: authority, ties, control leases, handoffs,
//! start state after failover.
//!
//! A [`Gate`] is the state machine for one index. It does no I/O and reads no clock:
//! the caller passes the home's monotonic time to each input. After each input, the
//! caller takes [`Gate::handoff`], records it in the index log before any frame that
//! input accepted, and publishes it on the control channel.

#![deny(clippy::wildcard_enum_match_arm)]

mod gate;

use std::fmt;

use types::name::Name;
use types::time::Span;

pub use gate::{Gate, Key};

/// How strongly a writer claims control. A higher authority takes control from a lower
/// one; an equal one waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Authority(pub u8);

impl Authority {
    /// The highest authority. Nothing can take control from it.
    pub const ABSOLUTE: Self = Self(u8::MAX);
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

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

/// A control lease: a holder that does not write for this long loses control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease(Span);

impl Lease {
    /// Makes a control lease of `span`.
    ///
    /// # Errors
    ///
    /// [`Error::Lease`] when `span` is not longer than zero.
    pub fn new(span: Span) -> Result<Self, Error> {
        if span <= Span::ZERO {
            return Err(Error::Lease { span });
        }
        Ok(Self(span))
    }

    /// The length of the control lease.
    #[must_use]
    pub fn span(self) -> Span {
        self.0
    }
}

/// A change of holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// The new holder, or `None` when the gate is now empty.
    pub to: Option<Writer>,
}

/// Why the gate refused a control lease or a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Another writer holds control.
    Waiting,
    /// The gate is held for the writer that held control before a restart, until it
    /// reopens or its grace ends.
    Reserved,
    /// The writer's control lease ran out. It stays out of the gate until it reopens.
    Expired,
    /// A control lease must be longer than zero.
    Lease {
        /// The span that was asked for.
        span: Span,
    },
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
            Self::Lease { span } => {
                write!(f, "control lease must be longer than zero, got {span}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    mod lease {
        use super::*;

        #[test]
        fn keeps_a_positive_span() {
            let lease = Lease::new(Span::NANOSECOND).expect("positive lease");
            assert_eq!(lease.span(), Span::NANOSECOND);
        }

        #[test]
        fn rejects_zero() {
            let err = Lease::new(Span::ZERO).expect_err("zero lease");
            assert_eq!(err, Error::Lease { span: Span::ZERO });
            assert_eq!(
                err.to_string(),
                "control lease must be longer than zero, got 0s"
            );
        }

        #[test]
        fn rejects_a_negative_span() {
            let span = Span::from_nanos(-1);
            assert_eq!(Lease::new(span), Err(Error::Lease { span }));
        }
    }

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
