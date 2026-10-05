use std::cmp::{self, Reverse};

use types::time::{Monotonic, Span};

use crate::drift::PER_NANO;
use crate::{Drift, Error, Measurement};

/// The offsets that every reading of one clock allows. Each reading, widened by drift,
/// holds the true offset, so the true offset lies where they all overlap. A read return
/// bounds the offset from above, and a stamp before a start command bounds it from
/// below, so readings that are open on one side still give a narrow overlap.
///
/// ```
/// use estimate::{Drift, Error, Overlap};
/// use types::time::{Monotonic, Span};
///
/// let ns = Span::from_nanos;
/// let mut overlap = Overlap::new(Drift::UNDISCIPLINED);
/// overlap.push_low(Monotonic(0), ns(4))?;
/// assert_eq!(overlap.at(Monotonic(0)), Err(Error::Open));
/// overlap.push_high(Monotonic(0), ns(14))?;
/// let now = overlap.at(Monotonic(0))?;
/// assert_eq!((now.offset(), now.error()), (ns(9), ns(5)));
/// # Ok::<(), estimate::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Overlap {
    drift: Drift,
    /// The local time of the newest reading, or zero before the first.
    newest: Monotonic,
    /// The reading whose low edge is highest from `newest` on.
    low: Option<Reading>,
    /// The reading whose high edge is lowest from `newest` on.
    high: Option<Reading>,
}

impl Overlap {
    /// Starts an empty overlap for a clock that drifts from mesh time by at most
    /// `drift`. Each edge must hold true mesh time, with the node's own error inside
    /// it, so `drift` covers only this clock.
    #[must_use]
    pub const fn new(drift: Drift) -> Self {
        Self {
            drift,
            newest: Monotonic(0),
            low: None,
            high: None,
        }
    }

    /// Narrows the overlap with one reading: at local time `at`, the offset is at
    /// least `low` and at most `high`. On an error, the overlap does not change.
    ///
    /// # Errors
    ///
    /// - [`Error::Backwards`] when `at` is older than the newest reading: the clock
    ///   restarted.
    /// - [`Error::Disjoint`] when the reading shares no offset with the overlap, or
    ///   `low` is above `high`: the clock jumped, or it drifts faster than `drift`.
    pub fn push(&mut self, at: Monotonic, low: Span, high: Span) -> Result<(), Error> {
        self.narrow(Reading {
            at,
            low: Some(low),
            high: Some(high),
        })
    }

    /// Narrows the overlap with a reading open above: at local time `at`, the offset
    /// is at least `low`. A stamp before a start command gives one.
    ///
    /// # Errors
    ///
    /// As [`Overlap::push`].
    pub fn push_low(&mut self, at: Monotonic, low: Span) -> Result<(), Error> {
        self.narrow(Reading {
            at,
            low: Some(low),
            high: None,
        })
    }

    /// Narrows the overlap with a reading open below: at local time `at`, the offset
    /// is at most `high`. A read return gives one for the newest sample the host
    /// knows.
    ///
    /// # Errors
    ///
    /// As [`Overlap::push`].
    pub fn push_high(&mut self, at: Monotonic, high: Span) -> Result<(), Error> {
        self.narrow(Reading {
            at,
            low: None,
            high: Some(high),
        })
    }

    /// The overlap at `now`, earlier or later than the readings. From the newest
    /// reading on, it is the overlap of all of them. Earlier, it can be wider.
    ///
    /// # Errors
    ///
    /// - [`Error::Open`] when no reading gave a low edge, or none gave a high edge.
    /// - [`Error::Bound`] when its error is more than 36500 days.
    pub fn at(&self, now: Monotonic) -> Result<Measurement, Error> {
        let edges = [self.low, self.high]
            .into_iter()
            .flatten()
            .map(|r| r.edges_at(now, self.drift));
        let low = edges.clone().filter_map(|(low, _)| low).max();
        let high = edges.filter_map(|(_, high)| high).min();
        let (Some(low), Some(high)) = (low, high) else {
            return Err(Error::Open);
        };
        // Whole nanoseconds, rounded outward.
        let (low, high) = (low.div_euclid(PER_NANO), -(-high).div_euclid(PER_NANO));
        Measurement::between(now, low, high)
    }

    fn narrow(&mut self, reading: Reading) -> Result<(), Error> {
        let at = reading.at;
        if at < self.newest {
            return Err(Error::Backwards {
                at,
                newest: self.newest,
            });
        }
        // From `at` on, every edge moves at `drift`, so this order holds. Of equal
        // edges, the older one stays. A missing edge is `None`, which never wins.
        let low_at = |r: Option<Reading>| r?.edges_at(at, self.drift).0;
        let high_at = |r: Option<Reading>| r?.edges_at(at, self.drift).1;
        let low = cmp::max_by_key(Some(reading), self.low, |&r| low_at(r));
        let high =
            cmp::max_by_key(Some(reading), self.high, |&r| high_at(r).map(Reverse));
        if low_at(low).zip(high_at(high)).is_some_and(|(l, h)| l > h) {
            return Err(Error::Disjoint);
        }
        self.newest = at;
        self.low = low;
        self.high = high;
        Ok(())
    }
}

/// What one reading says: at local time `at`, the offset is at least `low` and at
/// most `high`.
#[derive(Clone, Copy, Debug)]
struct Reading {
    at: Monotonic,
    low: Option<Span>,
    high: Option<Span>,
}

impl Reading {
    /// The edges at `now`, widened by drift, in billionths of a nanosecond.
    fn edges_at(self, now: Monotonic, drift: Drift) -> (Option<i128>, Option<i128>) {
        let growth = drift.over_exact(now.0.abs_diff(self.at.0));
        let scaled = |edge: Span| i128::from(edge.nanos()) * PER_NANO;
        (
            self.low.map(|low| scaled(low) - growth),
            self.high.map(|high| scaled(high) + growth),
        )
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Error, Measurement, Overlap};

    const SECOND_NS: u64 = 1_000_000_000;

    /// One reading for a test: its local time and its edges.
    #[derive(Clone, Copy, Debug)]
    enum Reading {
        Both(u64, i64, i64),
        Low(u64, i64),
        High(u64, i64),
    }

    impl Reading {
        fn at(self) -> u64 {
            match self {
                Self::Both(at, ..) | Self::Low(at, _) | Self::High(at, _) => at,
            }
        }

        fn edges(self) -> (Option<i64>, Option<i64>) {
            match self {
                Self::Both(_, low, high) => (Some(low), Some(high)),
                Self::Low(_, low) => (Some(low), None),
                Self::High(_, high) => (None, Some(high)),
            }
        }

        fn push(self, overlap: &mut Overlap) -> Result<(), Error> {
            let at = Monotonic(self.at());
            match self {
                Self::Both(_, low, high) => overlap.push(at, ns(low), ns(high)),
                Self::Low(_, low) => overlap.push_low(at, ns(low)),
                Self::High(_, high) => overlap.push_high(at, ns(high)),
            }
        }
    }

    fn ns(n: i64) -> Span {
        Span::from_nanos(n)
    }

    /// A two-sided reading: within `error` of `offset`.
    fn m(at: u64, offset: i64, error: i64) -> Reading {
        Reading::Both(at, offset - error, offset + error)
    }

    fn overlap(ppb: u32, pushes: &[Reading]) -> Result<Overlap, Error> {
        let mut overlap = Overlap::new(Drift::from_ppb(ppb)?);
        for &reading in pushes {
            reading.push(&mut overlap)?;
        }
        Ok(overlap)
    }

    fn pair(m: Measurement) -> (i64, i64) {
        (m.offset().nanos(), m.error().nanos())
    }

    /// The overlap of `pushes` at `now` as `(offset, error)`, with `ppb` of drift.
    fn check(ppb: u32, pushes: &[Reading], now: u64) -> Result<(i64, i64), Error> {
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

        #[test]
        fn narrows_only_the_top_with_a_high_edge() {
            let pushes = [m(0, 0, 10), Reading::High(0, 4)];
            assert_eq!(check(0, &pushes, 0), Ok((-3, 7)));
            let wider = [m(0, 0, 10), Reading::High(0, 12)];
            assert_eq!(check(0, &wider, 0), Ok((0, 10)));
        }

        #[test]
        fn narrows_only_the_bottom_with_a_low_edge() {
            let pushes = [m(0, 0, 10), Reading::Low(0, -4)];
            assert_eq!(check(0, &pushes, 0), Ok((3, 7)));
            let wider = [m(0, 0, 10), Reading::Low(0, -12)];
            assert_eq!(check(0, &wider, 0), Ok((0, 10)));
        }

        #[test]
        fn joins_a_low_edge_and_a_high_edge() {
            let pushes = [Reading::Low(0, 4), Reading::High(0, 14)];
            assert_eq!(check(0, &pushes, 0), Ok((9, 5)));
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
        fn bounds_a_sample_between_a_start_and_a_read() {
            let start = Reading::Low(0, 0);
            let read = [start, Reading::High(2 * SECOND_NS, 4_000)];
            assert_eq!(check(1_000, &read, SECOND_NS), Ok((2_000, 3_000)));
            let later = [read[0], read[1], Reading::High(3 * SECOND_NS, 2_000)];
            assert_eq!(check(1_000, &later, SECOND_NS), Ok((1_500, 2_500)));
        }

        #[test]
        fn keeps_the_other_edge_of_each_kept_reading() {
            let pushes = [m(0, 0, 100), Reading::High(10 * SECOND_NS, 9_000)];
            assert_eq!(check(1_000, &pushes, 0), Ok((0, 100)));
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

        #[test]
        fn moves_a_center_past_a_span_inside_it() {
            let (min, max) = (i64::MIN, i64::MAX);
            let low = [Reading::Low(0, min), Reading::High(SECOND_NS, min)];
            assert_eq!(check(1_000, &low, SECOND_NS), Ok((min, 1_000)));
            let high = [Reading::High(0, max), Reading::Low(SECOND_NS, max)];
            assert_eq!(check(1_000, &high, SECOND_NS), Ok((max, 1_000)));
        }

        #[test]
        fn fails_with_the_largest_span_for_a_wider_error() {
            let error = Span::from_nanos(i64::MAX);
            let all = [Reading::Both(0, i64::MIN, i64::MAX)];
            assert_eq!(check(0, &all, 0), Err(Error::Bound { error }));
        }
    }

    mod when_open {
        use super::*;

        #[test]
        fn fails_with_no_reading() {
            assert_eq!(check(0, &[], 0), Err(Error::Open));
            assert_eq!(
                Error::Open.to_string(),
                "overlap has no low edge or no high edge"
            );
        }

        #[test]
        fn fails_with_only_low_edges() {
            let pushes = [Reading::Low(0, 4), Reading::Low(5, 6)];
            assert_eq!(check(0, &pushes, 5), Err(Error::Open));
        }

        #[test]
        fn fails_with_only_high_edges() {
            let pushes = [Reading::High(0, 4), Reading::High(5, 6)];
            assert_eq!(check(0, &pushes, 5), Err(Error::Open));
        }
    }

    mod when_disjoint {
        use super::*;

        #[test]
        fn fails_and_keeps_the_overlap() {
            let mut overlap = overlap(0, &[m(0, 0, 5)]).expect("valid");
            assert_eq!(m(10, 11, 5).push(&mut overlap), Err(Error::Disjoint));
            assert_eq!(m(5, 4, 5).push(&mut overlap), Ok(()));
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

        #[test]
        fn fails_on_a_low_edge_above_its_high_edge() {
            let mut overlap = overlap(0, &[]).expect("valid");
            let crossed = Reading::Both(0, 5, 4);
            assert_eq!(crossed.push(&mut overlap), Err(Error::Disjoint));
            assert_eq!(overlap.at(Monotonic(0)), Err(Error::Open));
        }

        #[test]
        fn fails_on_one_edge_past_the_other_side() {
            let mut overlap = overlap(0, &[Reading::Low(0, 5)]).expect("valid");
            let below = Reading::High(0, 4);
            assert_eq!(below.push(&mut overlap), Err(Error::Disjoint));
            assert_eq!(Reading::High(0, 5).push(&mut overlap), Ok(()));
            assert_eq!(overlap.at(Monotonic(0)).map(pair), Ok((5, 0)));
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
            assert_eq!(m(9, 0, 5).push(&mut overlap), Err(backwards(9, 10)));
            assert_eq!(overlap.at(Monotonic(10)).map(pair), Ok((0, 5)));
            assert_eq!(
                backwards(9, 10).to_string(),
                "measurement at 9ns is older than the newest at 10ns"
            );
        }

        #[test]
        fn fails_on_a_one_sided_reading() {
            let mut overlap = overlap(0, &[Reading::Low(10, 0)]).expect("valid");
            let late = Reading::High(9, 5);
            assert_eq!(late.push(&mut overlap), Err(backwards(9, 10)));
            assert_eq!(overlap.at(Monotonic(10)), Err(Error::Open));
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
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;
        use crate::world::{TIME_NS, World, agreeing};

        /// `measurements` as readings in time order, open above for a side of 0, open
        /// below for 1, and closed for 2.
        fn readings(measurements: &[Measurement], sides: &[u8]) -> Vec<Reading> {
            let mut readings: Vec<_> = measurements
                .iter()
                .zip(sides)
                .map(|(measurement, side)| {
                    let (at, offset, error) = (
                        measurement.at().0,
                        measurement.offset().nanos(),
                        measurement.error().nanos(),
                    );
                    match side {
                        0 => Reading::Low(at, offset - error),
                        1 => Reading::High(at, offset + error),
                        _ => m(at, offset, error),
                    }
                })
                .collect();
            readings.sort_by_key(|r| r.at());
            readings
        }

        fn closed(readings: &[Reading]) -> bool {
            let edges = || readings.iter().map(|r| r.edges());
            edges().any(|e| e.0.is_some()) && edges().any(|e| e.1.is_some())
        }

        impl World {
            fn overlap(self, readings: &[Reading]) -> Overlap {
                let mut overlap = Overlap::new(self.drift);
                for reading in readings {
                    reading
                        .push(&mut overlap)
                        .expect("every reading holds the truth");
                }
                overlap
            }
        }

        /// 1 to 9 readings at any time, with any edges, either side open.
        fn any_readings() -> impl Strategy<Value = Vec<Reading>> {
            let one = (any::<u64>(), any::<i64>(), any::<i64>(), 0..3_u8).prop_map(
                |(at, low, high, side)| match side {
                    0 => Reading::Low(at, low),
                    1 => Reading::High(at, high),
                    _ => Reading::Both(at, low, high),
                },
            );
            vec(one, 1..10)
        }

        proptest! {
            #[test]
            fn holds_the_truth_at_any_time(
                (w, measurements) in agreeing(),
                sides in vec(0..3_u8, 9),
                t in any::<u64>(),
            ) {
                let readings = readings(&measurements, &sides);
                let now = w.overlap(&readings).at(Monotonic(t));
                if closed(&readings) {
                    let now = now.expect("narrow");
                    let truth = w.truth(t);
                    prop_assert!(w.holds_truth_at(now, t), "{now:?} misses {truth}");
                } else {
                    prop_assert_eq!(now, Err(Error::Open));
                }
            }

            #[test]
            fn is_the_overlap_of_every_reading_from_the_newest_on(
                (w, measurements) in agreeing(),
                sides in vec(0..3_u8, 9),
                later in 0..TIME_NS,
            ) {
                let readings = readings(&measurements, &sides);
                let newest = readings.last().expect("one or more").at();
                let now = newest + later;
                let (mut low, mut high) = (None, None);
                for r in &readings {
                    let growth = i128::from(w.drift.over(now - r.at()));
                    let (l, h) = r.edges();
                    if let Some(l) = l {
                        low = low.max(Some(i128::from(l) - growth));
                    }
                    if let Some(h) = h {
                        let h = i128::from(h) + growth;
                        high = Some(high.map_or(h, |high: i128| high.min(h)));
                    }
                }
                let all = match (low, high) {
                    (Some(low), Some(high)) => {
                        Measurement::between(Monotonic(now), low, high)
                    }
                    _ => Err(Error::Open),
                };
                prop_assert_eq!(w.overlap(&readings).at(Monotonic(now)), all);
            }

            #[test]
            fn never_panics_at_any_input(
                readings in any_readings(),
                now in any::<u64>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let mut readings = readings;
                readings.sort_by_key(|r| r.at());
                let drift = Drift::from_ppb(ppb).expect("valid");
                let mut overlap = Overlap::new(drift);
                for reading in readings {
                    match reading.push(&mut overlap) {
                        Ok(()) | Err(Error::Disjoint) => {}
                        Err(e @ (Error::Backwards { .. } | Error::Bound { .. }
                            | Error::Drift { .. } | Error::NoSources
                            | Error::NoMajority { .. } | Error::Open)) => {
                            prop_assert!(false, "unexpected {e}");
                        }
                    }
                }
                match overlap.at(Monotonic(now)) {
                    Ok(m) => prop_assert_eq!(m.at(), Monotonic(now)),
                    Err(Error::Bound { .. } | Error::Open) => {}
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
