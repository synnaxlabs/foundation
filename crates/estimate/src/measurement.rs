use types::time::{Monotonic, Span};

use crate::Drift;

/// The widest error bound, which means the offset is unknown. A wider bound stops here.
pub(crate) const MAX_ERROR: Span = Span::from_nanos(36_500 * Span::DAY.nanos());

/// What one time source says about a local clock, a node's or a device's: at local
/// time `at`, mesh time minus local time is within `error` of `offset`.
///
/// ```
/// use types::time::{Monotonic, Span};
///
/// let m = estimate::Measurement::new(Monotonic(10), Span::SECOND, Span::MILLISECOND);
/// assert_eq!(m.map(|m| m.offset()), Some(Span::SECOND));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Measurement {
    at: Monotonic,
    offset: Span,
    error: Span,
}

impl Measurement {
    /// Makes a measurement, or `None` when `error` is negative or more than 36500
    /// days.
    #[must_use]
    pub const fn new(at: Monotonic, offset: Span, error: Span) -> Option<Self> {
        if error.nanos() < 0 || error.nanos() > MAX_ERROR.nanos() {
            return None;
        }
        Some(Self { at, offset, error })
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
    /// [`Measurement::at`]: from zero to 36500 days. An estimate with 36500 days is
    /// unknown, and the true offset can be farther.
    #[must_use]
    pub const fn error(self) -> Span {
        self.error
    }

    /// The error bound at `now`, earlier or later than `at`: `error` plus what `drift`
    /// can add between them, up to 36500 days.
    #[must_use]
    pub fn error_at(self, now: Monotonic, drift: Drift) -> Span {
        capped(self.grown(now, drift))
    }

    /// The lowest and highest true offset at `now`, in nanoseconds. Unlike
    /// [`Measurement::error_at`], the error does not stop at 36500 days.
    pub(crate) fn bounds_at(self, now: Monotonic, drift: Drift) -> (i128, i128) {
        let error = self.grown(now, drift);
        let offset = i128::from(self.offset.nanos());
        (offset - error, offset + error)
    }

    fn grown(self, now: Monotonic, drift: Drift) -> i128 {
        let growth = drift.over(now.0.abs_diff(self.at.0));
        i128::from(self.error.nanos()) + i128::from(growth)
    }

    /// The measurement at `at` centered between `low` and `high`, with its offset
    /// saturated to a span. Its error covers both, up to 36500 days.
    ///
    /// # Panics
    ///
    /// When `low` is above `high`.
    pub(crate) fn between(at: Monotonic, low: i128, high: i128) -> Self {
        let (offset, error) = center(low, high);
        Self {
            at,
            offset,
            error: capped(error),
        }
    }

    /// As [`Measurement::between`], for a source that makes a measurement: an error
    /// over 36500 days fails.
    ///
    /// # Errors
    ///
    /// The error, saturated to a span, when it is over 36500 days.
    ///
    /// # Panics
    ///
    /// When `low` is above `high`.
    pub(crate) fn checked_between(
        at: Monotonic,
        low: i128,
        high: i128,
    ) -> Result<Self, Span> {
        let (offset, error) = center(low, high);
        let error = saturated(error);
        Self::new(at, offset, error).ok_or(error)
    }
}

/// The offset between `low` and `high`, saturated to a span, and the error that
/// covers both from it.
fn center(low: i128, high: i128) -> (Span, i128) {
    assert!(low <= high, "invariant: low {low}ns is above high {high}ns");
    let offset = saturated((low + high).div_euclid(2));
    let center = i128::from(offset.nanos());
    (offset, (high - center).max(center - low))
}

/// `nanos` as an error bound: 36500 days when it is wider.
fn capped(nanos: i128) -> Span {
    saturated(nanos.min(MAX_ERROR.nanos().into()))
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
    use crate::{Drift, Measurement};

    fn at(ns: u64, error: Span) -> Measurement {
        Measurement::new(Monotonic(ns), Span::ZERO, error).expect("valid")
    }

    mod new {
        use super::*;

        #[test]
        fn rejects_a_negative_error() {
            let error = Span::from_nanos(-1);
            assert_eq!(Measurement::new(Monotonic(0), Span::ZERO, error), None);
        }

        #[test]
        fn rejects_an_error_over_36500_days() {
            let error = Span::from_nanos(MAX_ERROR.nanos() + 1);
            assert_eq!(Measurement::new(Monotonic(0), Span::ZERO, error), None);
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
        fn stops_at_36500_days() {
            let under = |ns| at(0, Span::from_nanos(MAX_ERROR.nanos() - ns));
            let error = |m: Measurement, now| m.error_at(Monotonic(now), drift(1_000));
            let below = Span::from_nanos(MAX_ERROR.nanos() - 1);
            assert_eq!(error(under(1_001), SECOND_NS), below);
            assert_eq!(error(under(1_000), SECOND_NS), MAX_ERROR);
            assert_eq!(error(under(1_000), 2 * SECOND_NS), MAX_ERROR);
        }

        #[test]
        fn stops_at_the_widest_error_drift_and_time() {
            let error =
                at(0, MAX_ERROR).error_at(Monotonic(u64::MAX), drift(100_000_000));
            assert_eq!(error, MAX_ERROR);
        }

        #[test]
        fn bounds_past_36500_days() {
            let m = Measurement::new(Monotonic(0), Span::SECOND, MAX_ERROR);
            let m = m.expect("valid");
            let (offset, error) =
                (1_000_000_000, i128::from(MAX_ERROR.nanos()) + 1_000);
            assert_eq!(
                m.bounds_at(Monotonic(SECOND_NS), drift(1_000)),
                (offset - error, offset + error)
            );
        }
    }

    mod between {
        use super::*;

        #[test]
        fn stops_the_error_at_36500_days() {
            let widest = i128::from(MAX_ERROR.nanos());
            let m = Measurement::between(Monotonic(3), -widest - 1, widest + 1);
            assert_eq!(
                (m.at(), m.offset(), m.error()),
                (Monotonic(3), Span::ZERO, MAX_ERROR)
            );
        }

        #[test]
        fn fails_past_36500_days_when_checked() {
            let widest = i128::from(MAX_ERROR.nanos());
            let m = Measurement::checked_between(Monotonic(3), -widest, widest);
            let m = m.expect("36500 days");
            assert_eq!((m.offset(), m.error()), (Span::ZERO, MAX_ERROR));
            let err =
                Measurement::checked_between(Monotonic(3), -widest - 1, widest + 1);
            assert_eq!(err, Err(Span::from_nanos(MAX_ERROR.nanos() + 1)));
        }

        #[test]
        fn fails_when_checked_and_the_center_is_past_a_span() {
            let low = i128::from(i64::MIN) - i128::from(MAX_ERROR.nanos());
            let m = Measurement::checked_between(Monotonic(0), low, low);
            let m = m.expect("36500 days from the lowest span");
            assert_eq!(
                (m.offset(), m.error()),
                (Span::from_nanos(i64::MIN), MAX_ERROR)
            );
            let err = Measurement::checked_between(Monotonic(0), low - 1, low - 1);
            assert_eq!(err, Err(Span::from_nanos(MAX_ERROR.nanos() + 1)));
        }

        #[test]
        #[should_panic(expected = "invariant: low 1ns is above high 0ns")]
        fn panics_when_low_is_above_high() {
            let _ = Measurement::between(Monotonic(0), 1, 0);
        }

        #[test]
        #[should_panic(expected = "is above high 9223372036854775812ns")]
        fn panics_when_low_is_above_high_past_a_span() {
            let top = i128::from(i64::MAX);
            let _ = Measurement::between(Monotonic(0), top + 10, top + 5);
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::world::any_measurements;

        proptest! {
            #[test]
            fn error_at_is_at_most_36500_days(
                measurements in any_measurements(i64::MAX, MAX_ERROR.nanos()),
                now in any::<u64>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                for m in measurements {
                    let error = m.error_at(Monotonic(now), drift);
                    prop_assert!(m.error() <= error && error <= MAX_ERROR);
                }
            }
        }
    }
}
