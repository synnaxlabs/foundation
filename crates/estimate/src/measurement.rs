use types::time::{Interval, Monotonic, Span, Stamp};

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

    /// A measurement whose error is unknown: 36500 days. In
    /// [`combine`](crate::combine::combine) it votes only when no source has a known
    /// bound.
    #[must_use]
    pub const fn unknown(at: Monotonic, offset: Span) -> Self {
        Self {
            at,
            offset,
            error: MAX_ERROR,
        }
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

    /// Mesh time at [`Measurement::at`]: `at` plus the offset, within the error. With
    /// an error of 36500 days, the estimate is unknown and the true time can be outside
    /// the interval. An edge past the range of a stamp stops at the nearest stamp, so
    /// when mesh time is past that range, both edges are its end.
    #[must_use]
    pub fn interval(self) -> Interval {
        let time = i128::from(self.at.0) + i128::from(self.offset.nanos());
        let error = i128::from(self.error.nanos());
        Interval {
            earliest: Stamp::from_nanos(clamped(time - error)),
            latest: Stamp::from_nanos(clamped(time + error)),
        }
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

    /// Whether the measurement between `low` and `high` has an error of at most 36500
    /// days.
    ///
    /// # Panics
    ///
    /// When `low` is above `high`.
    pub(crate) fn fits(low: i128, high: i128) -> bool {
        center(low, high).1 <= MAX_ERROR.nanos().into()
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
    Span::from_nanos(clamped(nanos))
}

/// `nanos`, or the nearest `i64` when it is past that range.
fn clamped(nanos: i128) -> i64 {
    let nanos = nanos.clamp(i64::MIN.into(), i64::MAX.into());
    i64::try_from(nanos).expect("invariant: clamped to i64")
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

    mod unknown {
        use super::*;

        #[test]
        fn has_the_widest_error_that_new_takes() {
            let m = Measurement::unknown(Monotonic(5), Span::SECOND);
            assert_eq!((m.at(), m.offset()), (Monotonic(5), Span::SECOND));
            let wider = Span::from_nanos(m.error().nanos() + 1);
            assert_eq!(Measurement::new(m.at(), m.offset(), m.error()), Some(m));
            assert_eq!(Measurement::new(m.at(), m.offset(), wider), None);
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
        fn covers_a_center_past_a_span_with_the_error() {
            let low = i128::from(i64::MIN) - 5;
            let m = Measurement::between(Monotonic(0), low, low);
            let five = Span::from_nanos(5);
            assert_eq!((m.offset(), m.error()), (Span::from_nanos(i64::MIN), five));
            let low = i128::from(i64::MIN) - i128::from(MAX_ERROR.nanos()) - 1;
            let m = Measurement::between(Monotonic(0), low, low);
            assert_eq!(
                m,
                Measurement::unknown(Monotonic(0), Span::from_nanos(i64::MIN))
            );
        }

        #[test]
        fn fits_up_to_36500_days() {
            let widest = i128::from(MAX_ERROR.nanos());
            assert!(Measurement::fits(-widest, widest));
            assert!(!Measurement::fits(-widest - 1, widest + 1));
            assert!(!Measurement::fits(0, 2 * widest + 1));
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

    mod interval {
        use types::time::{Interval, Stamp};

        use super::*;

        fn interval(at: u64, offset: i64, error: i64) -> Interval {
            let m = Measurement::new(
                Monotonic(at),
                Span::from_nanos(offset),
                Span::from_nanos(error),
            );
            m.expect("valid").interval()
        }

        fn stamps(earliest: i64, latest: i64) -> Interval {
            Interval {
                earliest: Stamp::from_nanos(earliest),
                latest: Stamp::from_nanos(latest),
            }
        }

        #[test]
        fn is_at_plus_the_offset_within_the_error() {
            assert_eq!(interval(1_000, 300, 20), stamps(1_280, 1_320));
            assert_eq!(interval(1_000, -300, 20), stamps(680, 720));
            assert_eq!(interval(1_000, 0, 0), stamps(1_000, 1_000));
        }

        #[test]
        fn is_100_years_each_way_when_unknown() {
            let today = 1_791_158_400_000_000_000;
            let widest = MAX_ERROR.nanos();
            let unknown = interval(0, today, widest);
            assert_eq!(unknown, stamps(today - widest, today + widest));
        }

        #[test]
        fn stops_at_the_latest_stamp() {
            let top = i64::MAX.unsigned_abs();
            assert_eq!(interval(top - 2, 0, 5), stamps(i64::MAX - 7, i64::MAX));
            assert_eq!(interval(u64::MAX, 0, 5), stamps(i64::MAX, i64::MAX));
        }

        #[test]
        fn stops_at_the_earliest_stamp() {
            let widest = MAX_ERROR.nanos();
            assert_eq!(interval(2, i64::MIN, 5), stamps(i64::MIN, i64::MIN + 7));
            assert_eq!(
                interval(0, i64::MIN, widest),
                stamps(i64::MIN, i64::MIN + widest)
            );
        }
    }

    mod properties {
        use proptest::prelude::*;

        use types::time::Interval;

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

            #[test]
            fn interval_holds_at_plus_the_offset(
                measurements in any_measurements(i64::MAX, MAX_ERROR.nanos()),
            ) {
                for m in measurements {
                    let Interval { earliest, latest } = m.interval();
                    let (low, high) = (earliest.nanos(), latest.nanos());
                    let time = i128::from(m.at().0) + i128::from(m.offset().nanos());
                    let error = i128::from(m.error().nanos());
                    prop_assert!(low <= high);
                    if let Ok(time) = i64::try_from(time) {
                        prop_assert!(low <= time && time <= high);
                    }
                    let inside = |edge| edge != i64::MIN && edge != i64::MAX;
                    if inside(low) {
                        prop_assert_eq!(time - i128::from(low), error);
                    }
                    if inside(high) {
                        prop_assert_eq!(i128::from(high) - time, error);
                    }
                }
            }
        }
    }
}
