use std::cmp;

use types::time::Monotonic;

use crate::{Drift, Error, Measurement};

/// The offsets that every measurement of one clock allows. Each measurement, widened
/// by drift, holds the true offset, so the true offset lies where they all overlap.
/// Wide measurements whose true offsets sit at different places in their bounds give
/// a narrow overlap, as device reads do.
///
/// ```
/// use estimate::{Drift, Measurement, Overlap};
/// use types::time::{Monotonic, Span};
///
/// let ns = Span::from_nanos;
/// let read = |offset, error| Measurement::new(Monotonic(0), ns(offset), ns(error));
/// let mut overlap = Overlap::new(Drift::UNDISCIPLINED, read(5, 5)?);
/// overlap.push(read(13, 5)?)?;
/// let now = overlap.at(Monotonic(0))?;
/// assert_eq!((now.offset(), now.error()), (ns(9), ns(1)));
/// # Ok::<(), estimate::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Overlap {
    drift: Drift,
    newest: Monotonic,
    /// The measurement whose low edge is highest from `newest` on.
    low: Measurement,
    /// The measurement whose high edge is lowest from `newest` on.
    high: Measurement,
}

impl Overlap {
    /// Starts an overlap at `first`, for a clock that drifts from mesh time by at most
    /// `drift`. Each measurement must hold true mesh time, with the node's own error
    /// inside it, so `drift` covers only this clock.
    #[must_use]
    pub const fn new(drift: Drift, first: Measurement) -> Self {
        Self {
            drift,
            newest: first.at(),
            low: first,
            high: first,
        }
    }

    /// Narrows the overlap with `measurement`. On an error, the overlap does not
    /// change.
    ///
    /// # Errors
    ///
    /// - [`Error::Backwards`] when `measurement` is older than the newest one: the
    ///   clock restarted.
    /// - [`Error::Disjoint`] when it shares no offset with the overlap: the clock
    ///   jumped, or it drifts faster than `drift`.
    pub fn push(&mut self, measurement: Measurement) -> Result<(), Error> {
        let at = measurement.at();
        if at < self.newest {
            return Err(Error::Backwards {
                at,
                newest: self.newest,
            });
        }
        // From `at` on, every edge moves at `drift`, so this order holds. Of equal
        // edges, the older one stays.
        let bounds = |m: &Measurement| m.exact_bounds_at(at, self.drift);
        let low = cmp::max_by_key(measurement, self.low, |m| bounds(m).0);
        let high = cmp::min_by_key(self.high, measurement, |m| bounds(m).1);
        if bounds(&low).0 > bounds(&high).1 {
            return Err(Error::Disjoint);
        }
        self.newest = at;
        self.low = low;
        self.high = high;
        Ok(())
    }

    /// The overlap at `now`, earlier or later than the measurements. From the newest
    /// measurement on, it is the overlap of all of them. Earlier, it can be wider.
    ///
    /// # Errors
    ///
    /// [`Error::Bound`] when its error is more than 36500 days.
    pub fn at(&self, now: Monotonic) -> Result<Measurement, Error> {
        let low = self.low.bounds_at(now, self.drift);
        let high = self.high.bounds_at(now, self.drift);
        Measurement::between(now, low.0.max(high.0), low.1.min(high.1))
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Error, Measurement, Overlap};

    const SECOND_NS: u64 = 1_000_000_000;

    fn m(at: u64, offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    }

    /// The overlap of `pushes`, which holds at least one measurement.
    fn overlap(ppb: u32, pushes: &[Measurement]) -> Result<Overlap, Error> {
        let mut overlap = Overlap::new(Drift::from_ppb(ppb)?, pushes[0]);
        for &measurement in &pushes[1..] {
            overlap.push(measurement)?;
        }
        Ok(overlap)
    }

    fn pair(m: Measurement) -> (i64, i64) {
        (m.offset().nanos(), m.error().nanos())
    }

    /// The overlap of `pushes` at `now` as `(offset, error)`, with `ppb` of drift.
    fn check(ppb: u32, pushes: &[Measurement], now: u64) -> Result<(i64, i64), Error> {
        overlap(ppb, pushes)?.at(Monotonic(now)).map(pair)
    }

    mod at_one_time {
        use super::*;

        #[test]
        fn gives_one_measurement_as_it_is() {
            assert_eq!(check(0, &[m(5, 7, 3)], 5), Ok((7, 3)));
        }

        #[test]
        fn gives_the_intersection() {
            assert_eq!(check(0, &[m(0, 5, 5), m(0, 13, 5)], 0), Ok((9, 1)));
        }

        #[test]
        fn rounds_an_odd_width_outward() {
            assert_eq!(check(0, &[m(0, 5, 5), m(0, 12, 5)], 0), Ok((8, 2)));
            assert_eq!(check(0, &[m(0, -5, 5), m(0, -12, 5)], 0), Ok((-9, 2)));
        }

        #[test]
        fn accepts_bounds_that_only_touch() {
            assert_eq!(check(0, &[m(0, 0, 5), m(0, 10, 5)], 0), Ok((5, 0)));
        }
    }

    mod across_time {
        use super::*;

        #[test]
        fn widens_each_to_the_later_time() {
            let pushes = [m(0, 0, 0), m(SECOND_NS, 1_500, 1_000)];
            assert_eq!(check(1_000, &pushes, SECOND_NS), Ok((750, 250)));
        }

        #[test]
        fn widens_each_from_its_own_time() {
            let pushes = [m(0, 0, 0), m(SECOND_NS, 1_500, 1_000)];
            assert_eq!(check(1_000, &pushes, 2 * SECOND_NS), Ok((750, 1_250)));
            assert_eq!(check(1_000, &pushes, SECOND_NS / 2), Ok((250, 250)));
            assert_eq!(check(1_000, &pushes, 0), Ok((0, 0)));
        }

        #[test]
        fn keeps_an_old_precise_edge_until_drift_widens_it() {
            let precise = m(0, 0, 0);
            let soon = [precise, m(SECOND_NS, 0, 2_000)];
            assert_eq!(check(1_000, &soon, SECOND_NS), Ok((0, 1_000)));
            let later = [precise, m(10 * SECOND_NS, 0, 2_000)];
            assert_eq!(check(1_000, &later, 10 * SECOND_NS), Ok((0, 2_000)));
        }

        #[test]
        fn keeps_the_older_of_equal_edges() {
            for sign in [1, -1] {
                let (tied, other) = (
                    m(SECOND_NS, sign * 3_000, 5_000),
                    m(SECOND_NS, -sign * 2_000, 2_500),
                );
                let pushes = [m(0, 0, 1_000), tied, other];
                assert_eq!(check(1_000, &pushes, 0), Ok((0, 1_000)));
            }
        }

        #[test]
        fn orders_edges_exactly_near_a_tie() {
            let near = 10 * SECOND_NS + 5;
            for sign in [1, -1] {
                let pushes = [
                    m(0, sign * 20, 10),
                    m(near, sign * 10, 10),
                    m(near, -sign * 5, 15),
                ];
                assert_eq!(check(1, &pushes, near), Ok((sign * 5, 5)));
            }
        }

        #[test]
        fn fails_when_the_error_passes_36500_days() {
            let widest = MAX_ERROR.nanos();
            let error = Span::from_nanos(widest + 1);
            assert_eq!(
                check(1, &[m(0, 0, widest)], SECOND_NS),
                Err(Error::Bound { error })
            );
        }
    }

    mod when_disjoint {
        use super::*;

        #[test]
        fn fails_and_keeps_the_overlap() {
            let mut overlap = overlap(0, &[m(0, 0, 5)]).expect("valid");
            assert_eq!(overlap.push(m(10, 11, 5)), Err(Error::Disjoint));
            assert_eq!(overlap.push(m(5, 4, 5)), Ok(()));
            assert_eq!(overlap.at(Monotonic(5)).map(pair), Ok((2, 3)));
            assert_eq!(
                Error::Disjoint.to_string(),
                "measurement shares no offset with the overlap"
            );
        }

        #[test]
        fn fails_when_drift_cannot_cover_the_change() {
            let jump = [m(0, 0, 0), m(SECOND_NS, 1_001, 0)];
            assert_eq!(check(1_000, &jump, SECOND_NS), Err(Error::Disjoint));
            let most = [m(0, 0, 0), m(SECOND_NS, 1_000, 0)];
            assert_eq!(check(1_000, &most, SECOND_NS), Ok((1_000, 0)));
        }

        #[test]
        fn fails_on_a_gap_under_a_nanosecond() {
            assert_eq!(check(1, &[m(0, 0, 0), m(1, 1, 0)], 1), Err(Error::Disjoint));
        }
    }

    mod when_backwards {
        use super::*;

        fn backwards(at: u64, newest: u64) -> Error {
            Error::Backwards {
                at: Monotonic(at),
                newest: Monotonic(newest),
            }
        }

        #[test]
        fn fails_and_keeps_the_overlap() {
            let mut overlap = overlap(0, &[m(10, 0, 5)]).expect("valid");
            assert_eq!(overlap.push(m(9, 0, 5)), Err(backwards(9, 10)));
            assert_eq!(overlap.at(Monotonic(10)).map(pair), Ok((0, 5)));
            assert_eq!(
                backwards(9, 10).to_string(),
                "measurement at 9ns is older than the newest at 10ns"
            );
        }

        #[test]
        fn counts_a_measurement_that_narrows_nothing() {
            let pushes = [m(0, 0, 5), m(10, 0, 100), m(5, 0, 5)];
            assert_eq!(check(0, &pushes, 5), Err(backwards(5, 10)));
        }

        #[test]
        fn catches_a_contradiction_pushed_late() {
            let (a, b, c) =
                (m(0, 10_000, 0), m(0, 0, 0), m(100 * SECOND_NS, 0, 50_000));
            assert_eq!(check(1_000, &[a, b, c], 0), Err(Error::Disjoint));
            assert_eq!(
                check(1_000, &[c, a, b], 0),
                Err(backwards(0, 100 * SECOND_NS))
            );
        }

        #[test]
        fn catches_a_device_restart() {
            // The counter restarts at local 5 ms, so the true offset goes from 1 ms to
            // 6 ms, and the next read holds only the new one.
            let pushes = [
                m(4_000_000, 1_900_000, 1_000_000),
                m(5_000_000, 100_000, 1_000_000),
                m(1_000_000, 3_550_000, 2_500_000),
            ];
            let err = check(200_000, &pushes, 1_000_000);
            assert_eq!(err, Err(backwards(1_000_000, 5_000_000)));
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::world::{TIME_NS, World, agreeing, any_measurements};

        /// `pushes` in time order, as one clock gives them.
        fn in_order(mut pushes: Vec<Measurement>) -> Vec<Measurement> {
            pushes.sort_by_key(|m| m.at());
            pushes
        }

        impl World {
            fn overlap(self, pushes: &[Measurement]) -> Overlap {
                let mut overlap = Overlap::new(self.drift, pushes[0]);
                for &measurement in &pushes[1..] {
                    overlap
                        .push(measurement)
                        .expect("every measurement holds the truth");
                }
                overlap
            }
        }

        proptest! {
            #[test]
            fn holds_the_truth_at_any_time(
                (w, pushes) in agreeing(),
                t in any::<u64>(),
            ) {
                let overlap = w.overlap(&in_order(pushes));
                let now = overlap.at(Monotonic(t)).expect("narrow");
                prop_assert!(w.holds_truth_at(now, t), "{now:?} misses {}", w.truth(t));
            }

            #[test]
            fn is_the_overlap_of_every_measurement_from_the_newest_on(
                (w, pushes) in agreeing(),
                later in 0..TIME_NS,
            ) {
                let pushes = in_order(pushes);
                let newest = pushes.last().expect("one or more").at();
                let now = Monotonic(newest.0 + later);
                let (low, high) =
                    pushes.iter().fold((i128::MIN, i128::MAX), |(low, high), m| {
                        let bounds = m.bounds_at(now, w.drift);
                        (low.max(bounds.0), high.min(bounds.1))
                    });
                let all = Measurement::between(now, low, high);
                prop_assert_eq!(w.overlap(&pushes).at(now), all);
            }

            #[test]
            fn never_panics_at_any_input(
                pushes in any_measurements(i64::MAX, MAX_ERROR.nanos()),
                now in any::<u64>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let pushes = in_order(pushes);
                let drift = Drift::from_ppb(ppb).expect("valid");
                let mut overlap = Overlap::new(drift, pushes[0]);
                for &measurement in &pushes[1..] {
                    match overlap.push(measurement) {
                        Ok(()) | Err(Error::Disjoint) => {}
                        Err(e @ (Error::Backwards { .. } | Error::Bound { .. }
                            | Error::Drift { .. } | Error::NoSources
                            | Error::NoMajority { .. })) => {
                            prop_assert!(false, "unexpected {e}");
                        }
                    }
                }
                match overlap.at(Monotonic(now)) {
                    Ok(m) => prop_assert_eq!(m.at(), Monotonic(now)),
                    Err(Error::Bound { .. }) => {}
                    Err(e @ (Error::Backwards { .. } | Error::Disjoint
                        | Error::Drift { .. } | Error::NoSources
                        | Error::NoMajority { .. })) => {
                        prop_assert!(false, "unexpected {e}");
                    }
                }
            }
        }
    }
}
