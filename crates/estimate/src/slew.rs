use types::time::{Monotonic, Span};

use crate::{Drift, Measurement};

/// The fastest the served offset moves, in parts per million of local time.
const RATE_PPM: u64 = 500;

/// Mesh time that moves toward a target estimate at no more than 500 ppm, or steps
/// forward to it when mesh time is surely behind. It never goes back. Mesh time at a
/// local reading is the reading plus the offset served there. The part of the target
/// not yet applied goes into the error, so the estimate holds the true offset while
/// the slew runs. Every value is valid.
///
/// ```
/// use estimate::{Drift, Measurement, Slew};
/// use types::time::{Monotonic, Span};
///
/// let second = Monotonic(1_000_000_000);
/// let estimate = |offset, error| {
///     Measurement::new(second, offset, error).expect("valid")
/// };
/// let still = Drift::from_ppb(0).expect("valid");
/// let first = Slew::new(estimate(Span::ZERO, Span::MILLISECOND));
/// let slew = first.toward(second, still, estimate(Span::MILLISECOND, Span::ZERO));
/// let m = slew.at(Monotonic(2_000_000_000), still);
/// let half = Span::from_nanos(500_000);
/// assert_eq!((m.offset(), m.error()), (half, half));
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
    /// Serves `target` at once, at startup.
    #[must_use]
    pub const fn new(target: Measurement) -> Self {
        let (start, from) = (target.at(), target.offset());
        Self {
            start,
            from,
            target,
        }
    }

    /// Moves toward `target` from the offset served at `now`, for a local clock that
    /// drifts from mesh time by at most `drift`. When every offset `target` allows at
    /// `now` is above every offset `self` allows there, mesh time is behind, and the
    /// result steps to `target` at once. Mesh time from the result at or after `now`
    /// is never earlier than mesh time from `self` at or before `now`.
    #[must_use]
    pub fn toward(self, now: Monotonic, drift: Drift, target: Measurement) -> Self {
        let (earliest, _) = target.bounds_at(now, drift);
        let (_, latest) = self.bounds(now, drift);
        let from = if earliest > latest {
            target.offset()
        } else {
            Span::from_nanos(self.served(now))
        };
        Self {
            start: now,
            from,
            target,
        }
    }

    /// The estimate at `now`, for a local clock that drifts from mesh time by at most
    /// `drift`. Its offset is the one served at `now`. Its error adds the part of
    /// `target` not yet applied, up to 36500 days, so it holds the true offset when
    /// `target` does.
    #[must_use]
    pub fn at(self, now: Monotonic, drift: Drift) -> Measurement {
        let (low, high) = self.bounds(now, drift);
        Measurement::between(now, low, high)
    }

    /// The lowest and highest offset of the estimate at `now`, in nanoseconds, with
    /// no stop at 36500 days.
    fn bounds(self, now: Monotonic, drift: Drift) -> (i128, i128) {
        let served = i128::from(self.served(now));
        let (low, high) = self.target.bounds_at(now, drift);
        let half = (high - served).max(served - low);
        (served - half, served + half)
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
    use crate::{Drift, Measurement, Slew};

    const SECOND_NS: u64 = 1_000_000_000;

    fn estimate(at: u64, offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    }

    fn drift(ppb: u32) -> Drift {
        Drift::from_ppb(ppb).expect("valid")
    }

    /// The estimate at `now` as `(offset, error)`, with `ppb` of drift.
    fn check(slew: Slew, now: u64, ppb: u32) -> (i64, i64) {
        let m = slew.at(Monotonic(now), drift(ppb));
        assert_eq!(m.at(), Monotonic(now), "the estimate is at the reading");
        (m.offset().nanos(), m.error().nanos())
    }

    /// Offset zero, known within a second, so that a target within a second slews.
    fn unsure() -> Slew {
        Slew::new(estimate(0, 0, 1_000_000_000))
    }

    /// A slew at offset zero until one second, then toward `offset` with no error.
    fn from_zero(offset: i64) -> Slew {
        let now = Monotonic(SECOND_NS);
        unsure().toward(now, drift(0), estimate(SECOND_NS, offset, 0))
    }

    /// A slew at offset zero, known within 100 ns at zero, then at one second toward
    /// `offset` with an error of 10 ns, with `ppb` of drift.
    fn near_zero(offset: i64, ppb: u32) -> Slew {
        let now = Monotonic(SECOND_NS);
        let target = estimate(SECOND_NS, offset, 10);
        Slew::new(estimate(0, 0, 100)).toward(now, drift(ppb), target)
    }

    mod known_slews {
        use super::*;

        #[test]
        fn serves_the_first_estimate_at_once() {
            let slew = Slew::new(estimate(1_000, 5_000, 100));
            assert_eq!(check(slew, 1_000, 0), (5_000, 100));
            let start = (slew.start, slew.from);
            assert_eq!(start, (Monotonic(1_000), Span::from_nanos(5_000)));
        }

        #[test]
        fn keeps_mesh_time_when_the_target_changes() {
            assert_eq!(check(unsure(), SECOND_NS, 0), (0, 1_000_000_000));
            let after = from_zero(1_000_000);
            assert_eq!(check(after, SECOND_NS, 0), (0, 1_000_000));
            let start = (after.start, after.from);
            assert_eq!(start, (Monotonic(SECOND_NS), Span::ZERO));
        }

        #[test]
        fn continues_from_the_offset_served_mid_slew() {
            let now = Monotonic(2 * SECOND_NS);
            let slew = from_zero(1_000_000).toward(now, drift(0), estimate(0, 0, 0));
            assert_eq!(slew.from, Span::from_nanos(500_000));
            assert_eq!(check(slew, 2 * SECOND_NS, 0), (500_000, 500_000));
        }

        #[test]
        fn moves_500_microseconds_in_a_second() {
            let forward = from_zero(1_000_000);
            assert_eq!(check(forward, 2 * SECOND_NS, 0), (500_000, 500_000));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 2 * SECOND_NS, 0), (-500_000, 500_000));
        }

        #[test]
        fn rounds_the_served_offset_down() {
            let slew = from_zero(1_000_000);
            assert_eq!(check(slew, SECOND_NS + 1_999, 0), (0, 1_000_000));
            assert_eq!(check(slew, SECOND_NS + 2_000, 0), (1, 999_999));
        }

        #[test]
        fn stops_at_the_target() {
            let forward = from_zero(1_000_000);
            assert_eq!(check(forward, 3 * SECOND_NS, 0), (1_000_000, 0));
            assert_eq!(check(forward, 100 * SECOND_NS, 0), (1_000_000, 0));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 3 * SECOND_NS, 0), (-1_000_000, 0));
            assert_eq!(check(back, 100 * SECOND_NS, 0), (-1_000_000, 0));
        }

        #[test]
        fn adds_the_part_not_applied_to_the_drifted_error() {
            let now = Monotonic(SECOND_NS);
            let target = estimate(SECOND_NS, 1_000_000, 10);
            let slew = unsure().toward(now, drift(0), target);
            let error = 500_000 + 10 + 1_000;
            assert_eq!(check(slew, 2 * SECOND_NS, 1_000), (500_000, error));
        }

        #[test]
        fn steps_to_an_estimate_wholly_ahead() {
            let slew = near_zero(111, 0);
            let start = (slew.start, slew.from);
            assert_eq!(start, (Monotonic(SECOND_NS), Span::from_nanos(111)));
            assert_eq!(check(slew, SECOND_NS, 0), (111, 10));
        }

        #[test]
        fn slews_toward_an_estimate_that_overlaps() {
            assert_eq!(near_zero(110, 0).from, Span::ZERO);
            assert_eq!(check(near_zero(110, 0), SECOND_NS, 0), (0, 120));
        }

        #[test]
        fn counts_drift_before_it_steps() {
            assert_eq!(near_zero(120, 10).from, Span::ZERO);
            assert_eq!(near_zero(121, 10).from, Span::from_nanos(121));
        }

        #[test]
        fn never_steps_back() {
            let slew = near_zero(-1_000, 0);
            assert_eq!(slew.from, Span::ZERO);
            assert_eq!(check(slew, SECOND_NS, 0), (0, 1_010));
        }

        #[test]
        fn steps_only_past_the_whole_served_estimate() {
            let now = 2 * SECOND_NS;
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, now, 0), (-500_000, 500_000));
            let next = |offset| {
                back.toward(Monotonic(now), drift(0), estimate(now, offset, 10))
            };
            assert_eq!(next(10).from, Span::from_nanos(-500_000));
            assert_eq!(next(11).from, Span::from_nanos(11));
        }

        #[test]
        fn serves_from_at_and_before_the_start() {
            let slew = Slew {
                start: Monotonic(SECOND_NS),
                from: Span::from_nanos(7),
                target: estimate(SECOND_NS, 1_000_000, 0),
            };
            assert_eq!(check(slew, 0, 0), (7, 999_993));
            assert_eq!(check(slew, SECOND_NS, 0), (7, 999_993));
        }

        #[test]
        fn stops_the_error_at_36500_days() {
            let widest = MAX_ERROR.nanos();
            let slew = |error| Slew {
                start: Monotonic(0),
                from: Span::ZERO,
                target: estimate(0, 10, error),
            };
            assert_eq!(check(slew(widest - 11), 0, 0), (0, widest - 1));
            assert_eq!(check(slew(widest - 10), 0, 0), (0, widest));
            assert_eq!(check(slew(widest - 9), 0, 0), (0, widest));
        }

        #[test]
        fn stops_an_error_wider_than_a_span_at_36500_days() {
            let slew = Slew {
                start: Monotonic(0),
                from: Span::from_nanos(i64::MIN),
                target: estimate(0, i64::MAX, 0),
            };
            assert_eq!(check(slew, 0, 0), (i64::MIN, MAX_ERROR.nanos()));
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
            let m = slew.at(Monotonic(now), drift);
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
                let next = s.toward(Monotonic(switch), drift, target);
                let before = mesh(s, early, drift);
                prop_assert!(before <= mesh(s, late, drift));
                prop_assert!(before <= mesh(next, late, drift));
                let at_switch = |slew: Slew| mesh(slew, switch, drift);
                prop_assert!(at_switch(s) <= at_switch(next));
            }

            #[test]
            fn steps_only_to_a_target_wholly_ahead(
                s in slew(),
                switch in 0..TIME_NS,
                target in target(),
                ppb in 0..=100_000_000_u32,
            ) {
                let now = Monotonic(switch);
                let drift = Drift::from_ppb(ppb).expect("valid");
                let edges = |m: Measurement| {
                    let offset = i128::from(m.offset().nanos());
                    let error = i128::from(m.error_at(now, drift).nanos());
                    (offset - error, offset + error)
                };
                let served = s.at(now, drift);
                let ahead = edges(target).0 > edges(served).1;
                let expected = if ahead { target.offset() } else { served.offset() };
                let next = s.toward(now, drift, target);
                prop_assert_eq!(next.at(now, drift).offset(), expected);
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
                let m = slew.at(Monotonic(now), w.drift);
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
                let next = s.toward(Monotonic(now[0]), drift, target);
                for slew in [s, next] {
                    let m = slew.at(Monotonic(now[1]), drift);
                    prop_assert_eq!(m.at(), Monotonic(now[1]));
                    prop_assert!(m.error() <= MAX_ERROR);
                }
            }
        }
    }
}
