use types::time::{Monotonic, Span};

use crate::measurement::saturated;
use crate::{Drift, Error, Measurement};

/// The fastest the served offset moves, in parts per million of local time.
const RATE_PPM: u64 = 500;

/// Mesh time that moves toward a target estimate at no more than 500 ppm, so it never
/// goes back. Mesh time at a local reading is the reading plus the offset served
/// there. The part of the target not yet applied goes into the error, so the estimate
/// holds the true offset while the slew runs. Every value is valid.
///
/// ```
/// use estimate::{Drift, Measurement, Slew};
/// use types::time::{Monotonic, Span};
///
/// let second = Monotonic(1_000_000_000);
/// let ahead = Measurement::new(second, Span::MILLISECOND, Span::ZERO)?;
/// let slew = Slew::new(Measurement::new(second, Span::ZERO, Span::ZERO)?);
/// let slew = slew.toward(second, ahead);
/// let m = slew.at(Monotonic(2_000_000_000), Drift::from_ppb(0)?)?;
/// let half = Span::from_nanos(500_000);
/// assert_eq!((m.offset(), m.error()), (half, half));
/// # Ok::<(), estimate::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slew {
    /// The local monotonic reading when the slew started.
    pub start: Monotonic,
    /// The offset served at `start`, and before it.
    pub from: Span,
    /// The estimate that the served offset moves toward.
    pub target: Measurement,
}

impl Slew {
    /// Serves `target` at once: the one step, at startup.
    #[must_use]
    pub const fn new(target: Measurement) -> Self {
        let (start, from) = (target.at(), target.offset());
        Self {
            start,
            from,
            target,
        }
    }

    /// Moves toward `target` from the offset served at `now`. Mesh time from the result
    /// at or after `now` is never earlier than mesh time from `self` at or before
    /// `now`.
    #[must_use]
    pub fn toward(self, now: Monotonic, target: Measurement) -> Self {
        let from = Span::from_nanos(self.served(now));
        Self {
            start: now,
            from,
            target,
        }
    }

    /// The estimate at `now`, for a local clock that drifts from mesh time by at most
    /// `drift`. Its offset is the one served at `now`. Its error adds the part of
    /// `target` not yet applied, so it holds the true offset when `target` does.
    ///
    /// # Errors
    ///
    /// [`Error::Bound`] when the error is more than 36500 days.
    pub fn at(self, now: Monotonic, drift: Drift) -> Result<Measurement, Error> {
        let served = self.served(now);
        let gap = self.target.offset().nanos().abs_diff(served);
        let error = self.target.error_at(now, drift).nanos();
        let error = saturated(i128::from(error) + i128::from(gap));
        Measurement::new(now, Span::from_nanos(served), error)
    }

    /// The offset served at `now`, in nanoseconds: between `from` and the target's.
    fn served(self, now: Monotonic) -> i64 {
        let moved = now.0.saturating_sub(self.start.0) / (1_000_000 / RATE_PPM);
        let (from, to) = (self.from.nanos(), self.target.offset().nanos());
        if from <= to {
            from.saturating_add_unsigned(moved).min(to)
        } else {
            from.saturating_sub_unsigned(moved).max(to)
        }
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Error, Measurement, Slew};

    const SECOND_NS: u64 = 1_000_000_000;

    fn estimate(at: u64, offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    }

    /// The estimate at `now` as `(offset, error)`, with `ppb` of drift.
    fn check(slew: Slew, now: u64, ppb: u32) -> Result<(i64, i64), Error> {
        let m = slew.at(Monotonic(now), Drift::from_ppb(ppb)?)?;
        assert_eq!(m.at(), Monotonic(now), "the estimate is at the reading");
        Ok((m.offset().nanos(), m.error().nanos()))
    }

    /// A slew at offset zero until one second, then toward `offset` with no error.
    fn from_zero(offset: i64) -> Slew {
        let now = Monotonic(SECOND_NS);
        Slew::new(estimate(0, 0, 0)).toward(now, estimate(SECOND_NS, offset, 0))
    }

    mod known_slews {
        use super::*;

        #[test]
        fn serves_the_first_estimate_at_once() {
            let slew = Slew::new(estimate(1_000, 5_000, 100));
            assert_eq!(check(slew, 1_000, 0), Ok((5_000, 100)));
            let start = (slew.start, slew.from);
            assert_eq!(start, (Monotonic(1_000), Span::from_nanos(5_000)));
        }

        #[test]
        fn keeps_mesh_time_when_the_target_changes() {
            let before = Slew::new(estimate(0, 0, 0));
            assert_eq!(check(before, SECOND_NS, 0), Ok((0, 0)));
            let after = from_zero(1_000_000);
            assert_eq!(check(after, SECOND_NS, 0), Ok((0, 1_000_000)));
            let start = (after.start, after.from);
            assert_eq!(start, (Monotonic(SECOND_NS), Span::ZERO));
        }

        #[test]
        fn continues_from_the_offset_served_mid_slew() {
            let now = Monotonic(2 * SECOND_NS);
            let slew = from_zero(1_000_000).toward(now, estimate(0, 0, 0));
            assert_eq!(slew.from, Span::from_nanos(500_000));
            assert_eq!(check(slew, 2 * SECOND_NS, 0), Ok((500_000, 500_000)));
        }

        #[test]
        fn moves_500_microseconds_in_a_second() {
            let forward = from_zero(1_000_000);
            assert_eq!(check(forward, 2 * SECOND_NS, 0), Ok((500_000, 500_000)));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 2 * SECOND_NS, 0), Ok((-500_000, 500_000)));
        }

        #[test]
        fn rounds_the_served_offset_down() {
            let slew = from_zero(1_000_000);
            assert_eq!(check(slew, SECOND_NS + 1_999, 0), Ok((0, 1_000_000)));
            assert_eq!(check(slew, SECOND_NS + 2_000, 0), Ok((1, 999_999)));
        }

        #[test]
        fn stops_at_the_target() {
            let forward = from_zero(1_000_000);
            assert_eq!(check(forward, 3 * SECOND_NS, 0), Ok((1_000_000, 0)));
            assert_eq!(check(forward, 100 * SECOND_NS, 0), Ok((1_000_000, 0)));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 3 * SECOND_NS, 0), Ok((-1_000_000, 0)));
            assert_eq!(check(back, 100 * SECOND_NS, 0), Ok((-1_000_000, 0)));
        }

        #[test]
        fn adds_the_part_not_applied_to_the_drifted_error() {
            let now = Monotonic(SECOND_NS);
            let target = estimate(SECOND_NS, 1_000_000, 10);
            let slew = Slew::new(estimate(0, 0, 0)).toward(now, target);
            let error = 500_000 + 10 + 1_000;
            assert_eq!(check(slew, 2 * SECOND_NS, 1_000), Ok((500_000, error)));
        }

        #[test]
        fn serves_from_at_and_before_the_start() {
            let slew = Slew {
                start: Monotonic(SECOND_NS),
                from: Span::from_nanos(7),
                target: estimate(SECOND_NS, 1_000_000, 0),
            };
            assert_eq!(check(slew, 0, 0), Ok((7, 999_993)));
            assert_eq!(check(slew, SECOND_NS, 0), Ok((7, 999_993)));
        }

        #[test]
        fn fails_when_the_error_passes_36500_days() {
            let widest = MAX_ERROR.nanos();
            let slew = |error| Slew {
                start: Monotonic(0),
                from: Span::ZERO,
                target: estimate(0, 10, error),
            };
            assert_eq!(check(slew(widest - 10), 0, 0), Ok((0, widest)));
            let error = Span::from_nanos(widest + 1);
            assert_eq!(check(slew(widest - 9), 0, 0), Err(Error::Bound { error }));
        }

        #[test]
        fn saturates_an_error_wider_than_a_span() {
            let slew = Slew {
                start: Monotonic(0),
                from: Span::from_nanos(i64::MIN),
                target: estimate(0, i64::MAX, 0),
            };
            let error = Span::from_nanos(i64::MAX);
            assert_eq!(check(slew, 0, 0), Err(Error::Bound { error }));
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::world::{ERROR_NS, TIME_NS, truechimer, world};

        const OFFSET_NS: i64 = 1 << 50;

        /// A target with an offset and error small enough that every estimate is
        /// valid.
        fn target() -> impl Strategy<Value = Measurement> {
            let parts = (0..TIME_NS, -OFFSET_NS..OFFSET_NS, 0..ERROR_NS);
            parts.prop_map(|(at, offset, error)| estimate(at, offset, error))
        }

        fn slew() -> impl Strategy<Value = Slew> {
            (0..TIME_NS, -OFFSET_NS..OFFSET_NS, target()).prop_map(
                |(start, from, target)| Slew {
                    start: Monotonic(start),
                    from: Span::from_nanos(from),
                    target,
                },
            )
        }

        /// Mesh time at `now`: `now` plus the offset served there.
        fn mesh(slew: Slew, now: u64, drift: Drift) -> i128 {
            let m = slew.at(Monotonic(now), drift).expect("a valid estimate");
            i128::from(now) + i128::from(m.offset().nanos())
        }

        /// Three readings in order.
        fn readings() -> impl Strategy<Value = [u64; 3]> {
            any::<[u64; 3]>().prop_map(|r| {
                let mut r = r.map(|t| t % TIME_NS);
                r.sort_unstable();
                r
            })
        }

        proptest! {
            #[test]
            fn mesh_time_never_goes_back(
                s in slew(),
                [early, switch, late] in readings(),
                target in target(),
                ppb in 0..=100_000_000_u32,
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let next = s.toward(Monotonic(switch), target);
                let before = mesh(s, early, drift);
                prop_assert!(before <= mesh(s, late, drift));
                prop_assert!(before <= mesh(next, late, drift));
                let at_switch = |slew: Slew| mesh(slew, switch, drift);
                prop_assert_eq!(at_switch(s), at_switch(next));
            }

            #[test]
            fn holds_the_truth_when_the_target_does(
                (w, target) in world().prop_flat_map(|w| (Just(w), truechimer(w))),
                start in 0..TIME_NS,
                from in -OFFSET_NS..OFFSET_NS,
                now in 0..TIME_NS,
            ) {
                let (start, from) = (Monotonic(start), Span::from_nanos(from));
                let slew = Slew { start, from, target };
                let m = slew.at(Monotonic(now), w.drift).expect("a valid estimate");
                prop_assert!(w.holds_truth_at(m, now), "{m:?} misses {}", w.truth(now));
            }

            #[test]
            fn moves_at_most_500_ppm(s in slew(), [early, _, late] in readings()) {
                let drift = Drift::from_ppb(0).expect("valid");
                let served = |t: u64| mesh(s, t, drift) - i128::from(t);
                let moved = (served(late) - served(early)).abs();
                prop_assert!(moved <= i128::from((late - early).div_ceil(2_000)));
            }

            #[test]
            fn serves_between_from_and_the_target(s in slew(), now in 0..TIME_NS) {
                let drift = Drift::from_ppb(0).expect("valid");
                let served = mesh(s, now, drift) - i128::from(now);
                let (from, to) = (s.from.nanos(), s.target.offset().nanos());
                let (low, high) = (from.min(to), from.max(to));
                prop_assert!((i128::from(low)..=i128::from(high)).contains(&served));
            }

            #[test]
            fn never_panics_at_any_input(
                start in any::<u64>(),
                from in any::<i64>(),
                target in (any::<u64>(), any::<i64>(), 0..=MAX_ERROR.nanos()),
                now in any::<[u64; 2]>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let (at, offset, error) = target;
                let target = estimate(at, offset, error);
                let (start, from) = (Monotonic(start), Span::from_nanos(from));
                let s = Slew { start, from, target };
                let next = s.toward(Monotonic(now[0]), target);
                for slew in [s, next] {
                    match slew.at(Monotonic(now[1]), drift) {
                        Ok(m) => prop_assert_eq!(m.at(), Monotonic(now[1])),
                        Err(Error::Bound { .. }) => {}
                        Err(e @ (Error::Backwards { .. } | Error::Crossed
                            | Error::Disjoint | Error::Drift { .. } | Error::NoSources
                            | Error::NoMajority { .. } | Error::Open)) => {
                            prop_assert!(false, "unexpected {e}");
                        }
                    }
                }
            }
        }
    }
}
