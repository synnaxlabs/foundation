//! The control lease and the error of its constructor.

use std::fmt;

use types::time::Span;

/// A control lease: a holder that does not write for this long loses control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease(Span);

impl Lease {
    /// Makes a control lease of `span`.
    ///
    /// # Errors
    ///
    /// [`Error`] when `span` is not longer than zero.
    pub fn new(span: Span) -> Result<Self, Error> {
        if span <= Span::ZERO {
            return Err(Error { span });
        }
        Ok(Self(span))
    }

    /// The length of the control lease.
    #[must_use]
    pub fn span(self) -> Span {
        self.0
    }
}

/// Why a control lease was not made: its span is not longer than zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// The span that was asked for.
    pub span: Span,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "control lease must be longer than zero, got {}",
            self.span
        )
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_positive_span() {
        let lease = Lease::new(Span::NANOSECOND).expect("positive lease");
        assert_eq!(lease.span(), Span::NANOSECOND);
    }

    #[test]
    fn rejects_zero() {
        let err = Lease::new(Span::ZERO).expect_err("zero lease");
        assert_eq!(err, Error { span: Span::ZERO });
        assert_eq!(
            err.to_string(),
            "control lease must be longer than zero, got 0s"
        );
    }

    #[test]
    fn rejects_a_negative_span() {
        let span = Span::from_nanos(-1);
        let err = Lease::new(span).expect_err("negative lease");
        assert_eq!(err, Error { span });
        assert_eq!(
            err.to_string(),
            "control lease must be longer than zero, got -1ns"
        );
    }
}
