//! The retention policy: the most time a hold keeps a sample on the indexes it selects.

use std::fmt;

use types::name::Selector;
use types::time::Span;

/// Caps the holds on the indexes that `select` matches: past `keep` after its store
/// time, no hold keeps a sample, so the node may trim it when it needs disk. An index
/// that no policy selects has no time cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    keep: Span,
}

impl Policy {
    /// Makes a policy. A `keep` of zero is valid: then no hold keeps a sample after its
    /// store time, so a reader that is behind gets a gap for each sample that the node
    /// trims before the reader gets it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Negative`] when `keep` is below zero.
    pub fn new(select: Selector, keep: Span) -> Result<Self, Error> {
        if keep < Span::ZERO {
            return Err(Error::Negative(keep));
        }
        Ok(Self { select, keep })
    }

    /// The indexes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The most time a hold keeps a sample after its store time: zero or more.
    #[must_use]
    pub const fn keep(&self) -> Span {
        self.keep
    }
}

/// A keep time that makes no policy. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The keep time is below zero.
    Negative(Span),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Negative(keep) => {
                write!(f, "a retention keeps {keep}, which is below zero")
            }
        }
    }
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Negative(_) => "Write a keep time of zero or more",
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn select() -> Selector {
        Selector::new(["site_a.**"]).unwrap()
    }

    #[test]
    fn refuses_a_keep_time_below_zero() {
        let keep = Span::from_nanos(-1);
        let error = Policy::new(select(), keep).unwrap_err();
        assert_eq!(error, Error::Negative(keep));
        assert_eq!(
            error.to_string(),
            "a retention keeps -1ns, which is below zero"
        );
        assert_eq!(error.fix(), "Write a keep time of zero or more");
    }

    #[test]
    fn keeps_a_keep_time_of_zero_or_more() {
        for keep in [
            Span::ZERO,
            Span::NANOSECOND,
            Span::from_nanos(3 * Span::DAY.nanos()),
        ] {
            let policy = Policy::new(select(), keep).unwrap();
            assert_eq!((policy.select(), policy.keep()), (&select(), keep));
        }
    }
}
