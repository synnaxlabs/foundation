use std::cmp::{self, Reverse};

use types::time::Monotonic;

use crate::measurement::center;
use crate::{Drift, Error, Measurement};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// The offsets that every measurement of one device clock allows. Each measurement,
/// widened by drift to the same time, holds the true offset, so the fit is where they
/// all overlap. Wide measurements whose true offsets sit at different places in their
/// bounds give a narrow fit, as device reads do.
///
/// ```
/// use estimate::{Drift, Fit, Measurement};
/// use types::time::{Monotonic, Span};
///
/// let ns = Span::from_nanos;
/// let read = |offset, error| Measurement::new(Monotonic(0), ns(offset), ns(error));
/// let mut fit = Fit::new(Drift::UNDISCIPLINED);
/// fit.push(read(5, 5)?)?;
/// fit.push(read(13, 5)?)?;
/// let overlap = fit.overlap().expect("pushed");
/// assert_eq!(overlap.offset(), Span::from_nanos(9));
/// assert_eq!(overlap.error(), Span::from_nanos(1));
/// # Ok::<(), estimate::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    drift: Drift,
    edges: Option<Edges>,
}

/// The measurement whose low edge is highest and the one whose high edge is lowest, at
/// any time after both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edges {
    low: Measurement,
    high: Measurement,
}

impl Fit {
    /// Makes an empty fit for a clock that drifts from mesh time by at most `drift`.
    #[must_use]
    pub const fn new(drift: Drift) -> Self {
        Self { drift, edges: None }
    }

    /// Narrows the fit with `measurement`.
    ///
    /// # Errors
    ///
    /// [`Error::Disjoint`] when the measurement shares no offset with the fit: the
    /// clock jumped, or it drifts faster than the fit's drift. The fit does not
    /// change.
    pub fn push(&mut self, measurement: Measurement) -> Result<(), Error> {
        let edges = match self.edges {
            None => Edges {
                low: measurement,
                high: measurement,
            },
            Some(edges) => edges.narrowed(measurement, self.drift),
        };
        let (_, low, high) = edges.bounds(self.drift);
        if low > high {
            return Err(Error::Disjoint);
        }
        self.edges = Some(edges);
        Ok(())
    }

    /// The overlap as one measurement, or `None` before the first push. Its
    /// [`Measurement::error_at`] with the fit's drift gives the bound at any time.
    #[must_use]
    pub fn overlap(&self) -> Option<Measurement> {
        self.edges.map(|edges| edges.overlap(self.drift))
    }
}

impl Edges {
    fn narrowed(self, measurement: Measurement, drift: Drift) -> Self {
        // Past both measurements, every edge moves at `drift`, so an edge's value at
        // local time zero orders it exactly at every later time. Of equal edges, the
        // later one wins, so push order never matters.
        let ppb = i128::from(drift.ppb());
        let by_low = |m: &Measurement| {
            let (low, _) = m.bounds_at(m.at(), drift);
            (low * NANOS_PER_SECOND + ppb * i128::from(m.at().0), m.at())
        };
        let by_high = |m: &Measurement| {
            let (_, high) = m.bounds_at(m.at(), drift);
            let at = Reverse(m.at());
            (high * NANOS_PER_SECOND - ppb * i128::from(m.at().0), at)
        };
        Self {
            low: cmp::max_by_key(self.low, measurement, by_low),
            high: cmp::min_by_key(self.high, measurement, by_high),
        }
    }

    /// The newer edge's time, and the lowest and highest offsets at it.
    fn bounds(self, drift: Drift) -> (Monotonic, i128, i128) {
        let at = self.low.at().max(self.high.at());
        let (low, _) = self.low.bounds_at(at, drift);
        let (_, high) = self.high.bounds_at(at, drift);
        (at, low, high)
    }

    fn overlap(self, drift: Drift) -> Measurement {
        let (at, low, high) = self.bounds(drift);
        let (offset, error) = center(low, high);
        Measurement::new(at, offset, error)
            .expect("invariant: the overlap lies inside the newer edge's bound")
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::{Drift, Error, Fit, Measurement};

    const SECOND_NS: u64 = 1_000_000_000;

    fn m(at: u64, offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    }

    fn fit(ppb: u32, pushes: &[Measurement]) -> Result<Fit, Error> {
        let mut fit = Fit::new(Drift::from_ppb(ppb)?);
        for &measurement in pushes {
            fit.push(measurement)?;
        }
        Ok(fit)
    }

    /// The overlap as `(at, offset, error)` after `pushes` with `ppb` of drift.
    fn check(
        ppb: u32,
        pushes: &[Measurement],
    ) -> Result<Option<(u64, i64, i64)>, Error> {
        let overlap = fit(ppb, pushes)?.overlap();
        Ok(overlap.map(|o| (o.at().0, o.offset().nanos(), o.error().nanos())))
    }

    mod at_one_time {
        use super::*;

        #[test]
        fn is_empty_before_the_first_push() {
            assert_eq!(check(0, &[]), Ok(None));
        }

        #[test]
        fn gives_one_measurement_as_it_is() {
            assert_eq!(check(0, &[m(5, 7, 3)]), Ok(Some((5, 7, 3))));
        }

        #[test]
        fn gives_the_intersection() {
            assert_eq!(check(0, &[m(0, 5, 5), m(0, 13, 5)]), Ok(Some((0, 9, 1))));
        }

        #[test]
        fn rounds_an_odd_width_outward() {
            assert_eq!(check(0, &[m(0, 5, 5), m(0, 12, 5)]), Ok(Some((0, 8, 2))));
            assert_eq!(check(0, &[m(0, -5, 5), m(0, -12, 5)]), Ok(Some((0, -9, 2))));
        }

        #[test]
        fn accepts_bounds_that_only_touch() {
            assert_eq!(check(0, &[m(0, 0, 5), m(0, 10, 5)]), Ok(Some((0, 5, 0))));
        }
    }

    mod across_time {
        use super::*;

        #[test]
        fn widens_each_to_the_later_time() {
            let pushes = [m(0, 0, 0), m(SECOND_NS, 1_500, 1_000)];
            assert_eq!(check(1_000, &pushes), Ok(Some((SECOND_NS, 750, 250))));
        }

        #[test]
        fn keeps_an_old_precise_edge_until_drift_widens_it() {
            let precise = m(0, 0, 0);
            let soon = [precise, m(SECOND_NS, 0, 2_000)];
            assert_eq!(check(1_000, &soon), Ok(Some((0, 0, 0))));
            let later = [precise, m(10 * SECOND_NS, 0, 2_000)];
            assert_eq!(check(1_000, &later), Ok(Some((10 * SECOND_NS, 0, 2_000))));
        }

        #[test]
        fn gives_the_same_overlap_in_any_order() {
            let pushes = [m(SECOND_NS, 1_500, 1_000), m(0, 0, 0)];
            assert_eq!(check(1_000, &pushes), Ok(Some((SECOND_NS, 750, 250))));
        }

        #[test]
        fn prefers_the_later_of_equal_edges() {
            let (equal_low, equal_high) =
                ([m(0, -1, 4), m(7, 0, 5)], [m(0, 1, 4), m(7, 0, 5)]);
            for (pushes, overlap) in [(equal_low, (7, -1, 4)), (equal_high, (7, 1, 4))]
            {
                assert_eq!(check(0, &pushes), Ok(Some(overlap)));
                assert_eq!(check(0, &[pushes[1], pushes[0]]), Ok(Some(overlap)));
            }
        }
    }

    mod when_disjoint {
        use super::*;

        #[test]
        fn fails_and_keeps_the_fit() {
            let mut fit = fit(0, &[m(0, 0, 5)]).expect("one measurement");
            let before = fit;
            assert_eq!(fit.push(m(0, 11, 5)), Err(Error::Disjoint));
            assert_eq!(fit, before);
            assert_eq!(
                Error::Disjoint.to_string(),
                "measurement shares no offset with the fit"
            );
        }

        #[test]
        fn fails_when_drift_cannot_cover_the_change() {
            let jump = [m(0, 0, 0), m(SECOND_NS, 1_001, 0)];
            assert_eq!(check(1_000, &jump), Err(Error::Disjoint));
            let most = [m(0, 0, 0), m(SECOND_NS, 1_000, 0)];
            assert_eq!(check(1_000, &most), Ok(Some((SECOND_NS, 1_000, 0))));
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::measurement::MAX_ERROR;
        use crate::world::{World, agreeing, any_measurements};

        impl World {
            fn fit(self, pushes: &[Measurement]) -> Fit {
                let mut fit = Fit::new(self.drift);
                for &measurement in pushes {
                    fit.push(measurement)
                        .expect("every measurement holds the truth");
                }
                fit
            }
        }

        proptest! {
            #[test]
            fn holds_the_truth_at_any_time(
                (w, pushes) in agreeing(),
                t in any::<u64>(),
            ) {
                let overlap = w.fit(&pushes).overlap().expect("pushed");
                prop_assert!(
                    w.holds_truth_at(overlap, t),
                    "{overlap:?} misses {} at {t}",
                    w.truth(t)
                );
            }

            #[test]
            fn is_no_wider_than_any_measurement_at_its_time((w, pushes) in agreeing()) {
                let overlap = w.fit(&pushes).overlap().expect("pushed");
                for m in pushes {
                    let error = m.error_at(overlap.at(), w.drift);
                    prop_assert!(overlap.error() <= error, "{overlap:?} vs {m:?}");
                }
            }

            #[test]
            fn ignores_push_order(
                (w, pushes, shuffled) in agreeing().prop_flat_map(|(w, p)| {
                    (Just(w), Just(p.clone()), Just(p).prop_shuffle())
                }),
            ) {
                prop_assert_eq!(w.fit(&pushes).overlap(), w.fit(&shuffled).overlap());
            }

            #[test]
            fn never_panics_at_any_input(
                pushes in any_measurements(i64::MAX, MAX_ERROR.nanos()),
                now in any::<u64>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let mut fit = Fit::new(drift);
                for measurement in pushes {
                    match fit.push(measurement) {
                        Ok(()) | Err(Error::Disjoint) => {}
                        Err(e @ (Error::Bound { .. } | Error::Drift { .. }
                            | Error::NoSources | Error::NoMajority { .. })) => {
                            prop_assert!(false, "unexpected {e}");
                        }
                    }
                }
                let overlap = fit.overlap().expect("the first push always fits");
                let error = overlap.error_at(Monotonic(now), drift);
                prop_assert!(error >= overlap.error(), "{overlap:?} at {now}");
            }
        }
    }
}
