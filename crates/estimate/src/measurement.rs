use types::time::{Monotonic, Span};

use crate::{Drift, Error};

/// The widest error bound. With the largest drift over the longest time, a widened
/// bound still fits in a [`Span`].
pub(crate) const MAX_ERROR: Span = Span::from_nanos(36_500 * Span::DAY.nanos());

/// What one time source says about a local clock, a node's or a device's: at local
/// time `at`, mesh time minus local time is within `error` of `offset`.
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
    /// [`Error::Bound`] when `error` is negative or more than 36500 days.
    pub const fn new(at: Monotonic, offset: Span, error: Span) -> Result<Self, Error> {
        if error.nanos() < 0 || error.nanos() > MAX_ERROR.nanos() {
            return Err(Error::Bound { error });
        }
        Ok(Self { at, offset, error })
    }

    /// The local time of the measurement.
    #[must_use]
    pub const fn at(self) -> Monotonic {
        self.at
    }

    /// Mesh time minus local time.
    #[must_use]
    pub const fn offset(self) -> Span {
        self.offset
    }

    /// How far the true offset can be from [`Measurement::offset`] at
    /// [`Measurement::at`]: from zero to 36500 days.
    #[must_use]
    pub const fn error(self) -> Span {
        self.error
    }

    /// The error bound at `now`, earlier or later than `at`: `error` plus what `drift`
    /// can add between them.
    #[must_use]
    pub fn error_at(self, now: Monotonic, drift: Drift) -> Span {
        let growth = drift.over(now.0.abs_diff(self.at.0));
        Span::from_nanos(self.error.nanos() + growth)
    }

    /// The lowest and highest true offset at `now`, in nanoseconds.
    pub(crate) fn bounds_at(self, now: Monotonic, drift: Drift) -> (i128, i128) {
        let error = i128::from(self.error_at(now, drift).nanos());
        let offset = i128::from(self.offset.nanos());
        (offset - error, offset + error)
    }

    /// The measurement at `at` that covers every offset from `low` to `high`, rounded
    /// outward. Its offset is the midpoint, or the nearest span when the midpoint is
    /// past a span's range.
    ///
    /// # Errors
    ///
    /// [`Error::Bound`] when the error is more than 36500 days. An error past a span's
    /// range reads as the largest span.
    pub(crate) fn between(at: Monotonic, low: i128, high: i128) -> Result<Self, Error> {
        let offset = saturated((low + high).div_euclid(2));
        let center = i128::from(offset.nanos());
        Self::new(at, offset, saturated((high - center).max(center - low)))
    }
}

/// `nanos` as a span, or the nearest span when it is past a span's range.
fn saturated(nanos: i128) -> Span {
    let nanos = nanos.clamp(i64::MIN.into(), i64::MAX.into());
    Span::from_nanos(i64::try_from(nanos).expect("invariant: clamped to i64"))
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use super::MAX_ERROR;
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
            assert_eq!(err, Err(Error::Bound { error }));
            assert_eq!(
                Error::Bound { error }.to_string(),
                "error bound -1ns is not between 0s and 36500d"
            );
        }

        #[test]
        fn rejects_an_error_over_36500_days() {
            let error = Span::from_nanos(MAX_ERROR.nanos() + 1);
            let err = Measurement::new(Monotonic(0), Span::ZERO, error);
            assert_eq!(err, Err(Error::Bound { error }));
        }

        #[test]
        fn takes_zero_and_36500_days() {
            assert_eq!(at(0, Span::ZERO).error(), Span::ZERO);
            assert_eq!(at(0, MAX_ERROR).error(), MAX_ERROR);
        }
    }

    mod error_at {
        use super::*;

        const SECOND_NS: u64 = 1_000_000_000;

        fn drift(ppb: u32) -> Drift {
            Drift::from_ppb(ppb).expect("valid")
        }

        #[test]
        fn is_the_error_at_the_measurement() {
            let m = at(5, Span::MICROSECOND);
            assert_eq!(m.error_at(Monotonic(5), drift(100_000)), Span::MICROSECOND);
        }

        #[test]
        fn grows_with_drift_after_the_measurement() {
            let m = at(0, Span::MICROSECOND);
            let error = m.error_at(Monotonic(SECOND_NS), drift(100_000));
            assert_eq!(error, Span::from_nanos(101_000));
        }

        #[test]
        fn grows_with_drift_before_the_measurement() {
            let m = at(SECOND_NS, Span::MICROSECOND);
            let error = m.error_at(Monotonic(0), drift(100_000));
            assert_eq!(error, Span::from_nanos(101_000));
        }

        #[test]
        fn fits_at_the_widest_error_drift_and_time() {
            let error =
                at(0, MAX_ERROR).error_at(Monotonic(u64::MAX), drift(100_000_000));
            assert_eq!(error, Span::from_nanos(4_998_274_407_370_955_162));
        }
    }
}
