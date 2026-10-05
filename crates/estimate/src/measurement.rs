use types::time::{Monotonic, Span};

use crate::{Drift, Error};

/// What one time source says at one moment: at local monotonic time `at`, mesh time
/// minus local monotonic time is within `error` of `offset`.
///
/// ```
/// use types::time::{Monotonic, Span};
///
/// let m = estimate::Measurement::new(Monotonic(10), Span::SECOND, Span::MILLISECOND)?;
/// assert_eq!(m.offset(), Span::SECOND);
/// # Ok::<(), estimate::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Measurement {
    at: Monotonic,
    offset: Span,
    error: Span,
}

impl Measurement {
    /// Makes a measurement.
    ///
    /// # Errors
    ///
    /// [`Error::NegativeBound`] when `error` is negative.
    pub const fn new(at: Monotonic, offset: Span, error: Span) -> Result<Self, Error> {
        if error.nanos() < 0 {
            return Err(Error::NegativeBound { error });
        }
        Ok(Self { at, offset, error })
    }

    /// The local monotonic time of the measurement.
    #[must_use]
    pub const fn at(self) -> Monotonic {
        self.at
    }

    /// Mesh time minus local monotonic time.
    #[must_use]
    pub const fn offset(self) -> Span {
        self.offset
    }

    /// How far the true offset can be from [`Measurement::offset`] at
    /// [`Measurement::at`]. It is never negative.
    #[must_use]
    pub const fn error(self) -> Span {
        self.error
    }

    /// The error bound at `now`, earlier or later than `at`: `error` plus what `drift`
    /// can add between them. It saturates at the largest span.
    #[must_use]
    pub fn error_at(self, now: Monotonic, drift: Drift) -> Span {
        let error = self.widened(now, drift);
        Span::from_nanos(i64::try_from(error).unwrap_or(i64::MAX))
    }

    /// The lowest and highest true offset at `now`, in nanoseconds.
    pub(crate) fn bounds_at(self, now: Monotonic, drift: Drift) -> (i128, i128) {
        let error = i128::try_from(self.widened(now, drift))
            .expect("invariant: an i64 plus a u64 times a u32 fits in i128");
        let offset = i128::from(self.offset.nanos());
        (offset - error, offset + error)
    }

    fn widened(self, now: Monotonic, drift: Drift) -> u128 {
        let error = u128::try_from(self.error.nanos())
            .expect("invariant: Measurement::new rejects a negative error");
        error + drift.over(now.0.abs_diff(self.at.0))
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::{Drift, Error, Measurement};

    fn at(ns: u64, error: Span) -> Measurement {
        Measurement::new(Monotonic(ns), Span::ZERO, error).expect("valid")
    }

    mod new {
        use super::*;

        #[test]
        fn rejects_a_negative_error() {
            let error = Span::from_nanos(-1);
            let err = Measurement::new(Monotonic(0), Span::ZERO, error);
            assert_eq!(err, Err(Error::NegativeBound { error }));
            assert_eq!(
                Error::NegativeBound { error }.to_string(),
                "error bound -1ns is negative"
            );
        }

        #[test]
        fn takes_a_zero_error() {
            assert_eq!(at(0, Span::ZERO).error(), Span::ZERO);
        }
    }

    mod error_at {
        use super::*;

        const DRIFT: Drift = Drift::from_ppb(100_000);
        const SECOND_NS: u64 = 1_000_000_000;

        #[test]
        fn is_the_error_at_the_measurement() {
            let m = at(5, Span::MICROSECOND);
            assert_eq!(m.error_at(Monotonic(5), DRIFT), Span::MICROSECOND);
        }

        #[test]
        fn grows_with_drift_after_the_measurement() {
            let m = at(0, Span::MICROSECOND);
            assert_eq!(
                m.error_at(Monotonic(SECOND_NS), DRIFT),
                Span::from_nanos(101_000)
            );
        }

        #[test]
        fn grows_with_drift_before_the_measurement() {
            let m = at(SECOND_NS, Span::MICROSECOND);
            assert_eq!(m.error_at(Monotonic(0), DRIFT), Span::from_nanos(101_000));
        }

        #[test]
        fn saturates_at_the_largest_span() {
            let m = at(0, Span::from_nanos(i64::MAX));
            let most = Drift::from_ppb(u32::MAX);
            assert_eq!(
                m.error_at(Monotonic(u64::MAX), most),
                Span::from_nanos(i64::MAX)
            );
        }
    }
}
