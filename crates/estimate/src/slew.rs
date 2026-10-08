use types::time::{Monotonic, Span, Stamp};

use crate::drift::PER_NANO;
use crate::{Drift, Measurement};

/// The fastest the served offset moves, in parts per million of local time.
const RATE_PPM: u64 = 500;

/// The local time, in nanoseconds, over which the served offset moves 1 ns.
const TICK_NS: u64 = 1_000_000 / RATE_PPM;

/// The largest gap to a target's earliest offset that mesh time slews across: one
/// second of slew. Past it, mesh time steps.
const STEP_GAP: Span = Span::from_nanos(500_000);

/// Mesh time that moves toward a target estimate at no more than 500 ppm, or steps
/// forward when it is more than 500 us behind every offset the target allows. It
/// never goes back. Mesh time at a local reading is the reading plus the offset served
/// there. The part of the target not yet applied goes into the error, so the estimate
/// holds the true offset while the slew runs. Every value is valid.
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
/// let half = Span::from_nanos(500_000);
/// let slew = first.toward(second, still, estimate(half, Span::ZERO));
/// let m = slew.at(Monotonic(1_500_000_000), still);
/// let quarter = Span::from_nanos(250_000);
/// assert_eq!((m.offset(), m.error()), (quarter, quarter));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slew {
    /// The local monotonic reading from which the served offset moves away from
    /// `from`.
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
    /// drifts from mesh time by at most `drift`. When the served offset is more than
    /// 500 us below the earliest offset `target` allows at `now`, the result steps to
    /// that earliest offset at once. The target's error counts with its full growth,
    /// past 36500 days. Mesh time from the result at or after `now` is never earlier
    /// than mesh time from `self` at or before `now`. When it does not step, the result
    /// keeps the readings of `self` where the served offset can move, 2 us apart, so
    /// calling it often does not slow the slew.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "the earliest offset is above the served one and at most the target's"
    )]
    pub fn toward(self, now: Monotonic, drift: Drift, target: Measurement) -> Self {
        let served = self.served(now);
        let (earliest, _) = target.bounds_at(now, drift);
        let step = earliest - i128::from(served) > i128::from(STEP_GAP.nanos());
        let (start, from) = if step {
            let fits = i64::try_from(earliest);
            let earliest = fits.unwrap_or_else(|_| {
                panic!("invariant: offset {earliest}ns is not an i64")
            });
            (now, earliest)
        } else {
            (self.ticks(now).1, served)
        };
        Self {
            start,
            from: Span::from_nanos(from),
            target,
        }
    }

    /// The estimate at `now`, for a local clock that drifts from mesh time by at most
    /// `drift`. Its offset is the one served at `now`. Its error adds the part of
    /// `target` not yet applied, up to 36500 days, so it holds the true offset when
    /// `target` does.
    #[must_use]
    pub fn at(self, now: Monotonic, drift: Drift) -> Measurement {
        let (low, high) = self.bounds_at(now, drift);
        Measurement::between(now, low, high)
    }

    /// The first reading at or after `now` at which the latest edge of mesh time is at
    /// or after `at`, for a local clock that drifts from mesh time by at most `drift`.
    /// At each reading from `now` to it, the edge is before `at`. It is `now` when the
    /// edge at `now` has reached `at`. Each stamp has one: at the last reading, the
    /// edge is at the end of a stamp's range.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "no step passes the last reading, which reaches each stamp"
    )]
    pub fn reach(self, now: Monotonic, at: Stamp, drift: Drift) -> Monotonic {
        // Over `j` ns the edge moves up by at most `j * (1 + rate) + 1` ns, the 1 for
        // the rounding of the growth and of the ticks, so no reading inside a step
        // reaches `at`. A downward slew moves the edge back at each tick, so a search
        // that takes the edge as monotonic can pass the first reading.
        let rate = i128::from(drift.ppb()).max(i128::from(RATE_PPM) * 1_000);
        let mut reading = now;
        loop {
            let latest = self.at(reading, drift).interval().latest;
            let gap = i128::from(at.nanos()) - i128::from(latest.nanos());
            if gap <= 0 {
                return reading;
            }
            let step = -(-(gap - 1) * PER_NANO).div_euclid(PER_NANO + rate);
            let step = u64::try_from(step.max(1)).expect("invariant: a gap fits a u64");
            let next = reading.0.checked_add(step);
            reading =
                Monotonic(next.expect("invariant: the last reading reaches `at`"));
        }
    }

    /// The lowest and highest offset of the estimate at `now`, in nanoseconds, with
    /// no stop at 36500 days.
    fn bounds_at(self, now: Monotonic, drift: Drift) -> (i128, i128) {
        let served = i128::from(self.served(now));
        let (low, high) = self.target.bounds_at(now, drift);
        let half = (high - served).max(served - low);
        (served - half, served + half)
    }

    /// The offset served at `now`, in nanoseconds: between `from` and the target's.
    fn served(self, now: Monotonic) -> i64 {
        let (moved, _) = self.ticks(now);
        let (from, to) = (self.from.nanos(), self.target.offset().nanos());
        if from <= to {
            from.saturating_add_unsigned(moved).min(to)
        } else {
            from.saturating_sub_unsigned(moved).max(to)
        }
    }

    /// The whole ticks from `start` to `now`, and the reading where the last one ends:
    /// `start` with none, or `now` before `start`.
    fn ticks(self, now: Monotonic) -> (u64, Monotonic) {
        let elapsed = now.0.saturating_sub(self.start.0);
        (elapsed / TICK_NS, Monotonic(now.0 - elapsed % TICK_NS))
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span, Stamp};

    use super::STEP_GAP;
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

    /// Offset zero, known within a second.
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
            let after = from_zero(500_000);
            assert_eq!(check(after, SECOND_NS, 0), (0, 500_000));
            let start = (after.start, after.from);
            assert_eq!(start, (Monotonic(SECOND_NS), Span::ZERO));
        }

        #[test]
        fn continues_from_the_offset_served_mid_slew() {
            let now = Monotonic(2 * SECOND_NS);
            let slew = from_zero(-1_000_000).toward(now, drift(0), estimate(0, 0, 0));
            assert_eq!(slew.from, Span::from_nanos(-500_000));
            assert_eq!(check(slew, 2 * SECOND_NS, 0), (-500_000, 500_000));
        }

        #[test]
        fn moves_500_microseconds_in_a_second() {
            let forward = from_zero(500_000);
            assert_eq!(check(forward, 3 * SECOND_NS / 2, 0), (250_000, 250_000));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 2 * SECOND_NS, 0), (-500_000, 500_000));
        }

        #[test]
        fn rounds_the_served_offset_down() {
            let slew = from_zero(500_000);
            assert_eq!(check(slew, SECOND_NS + 1_999, 0), (0, 500_000));
            assert_eq!(check(slew, SECOND_NS + 2_000, 0), (1, 499_999));
        }

        #[test]
        fn keeps_the_grid_of_the_old_slew() {
            let target = estimate(SECOND_NS, 500_000, 0);
            let again = |at| from_zero(500_000).toward(Monotonic(at), drift(0), target);
            let late = again(SECOND_NS + 1_999);
            assert_eq!((late.start, late.from), (Monotonic(SECOND_NS), Span::ZERO));
            assert_eq!(check(late, SECOND_NS + 2_000, 0), (1, 499_999));
            let moved = again(SECOND_NS + 2_001);
            let start = (moved.start, moved.from);
            assert_eq!(start, (Monotonic(SECOND_NS + 2_000), Span::from_nanos(1)));
        }

        #[test]
        fn stops_at_the_target() {
            let forward = from_zero(500_000);
            assert_eq!(check(forward, 2 * SECOND_NS, 0), (500_000, 0));
            assert_eq!(check(forward, 100 * SECOND_NS, 0), (500_000, 0));
            let back = from_zero(-1_000_000);
            assert_eq!(check(back, 3 * SECOND_NS, 0), (-1_000_000, 0));
            assert_eq!(check(back, 100 * SECOND_NS, 0), (-1_000_000, 0));
        }

        #[test]
        fn adds_the_part_not_applied_to_the_drifted_error() {
            let now = Monotonic(SECOND_NS);
            let target = estimate(SECOND_NS, -1_000_000, 10);
            let slew = unsure().toward(now, drift(0), target);
            let error = 500_000 + 10 + 1_000;
            assert_eq!(check(slew, 2 * SECOND_NS, 1_000), (-500_000, error));
        }

        #[test]
        fn steps_only_past_a_gap_of_500_microseconds() {
            assert_eq!(near_zero(500_010, 0).from, Span::ZERO);
            assert_eq!(near_zero(500_011, 0).from, Span::from_nanos(500_001));
        }

        #[test]
        fn steps_to_the_earliest_offset() {
            let slew = near_zero(1_000_000, 0);
            let start = (slew.start, slew.from);
            assert_eq!(start, (Monotonic(SECOND_NS), Span::from_nanos(999_990)));
            assert_eq!(check(slew, SECOND_NS, 0), (999_990, 20));
            assert_eq!(check(slew, SECOND_NS + 20_000, 0), (1_000_000, 10));
        }

        #[test]
        fn starts_a_step_at_now() {
            let now = SECOND_NS + 1_999;
            let slew = Slew::new(estimate(0, 0, 100));
            let next =
                slew.toward(Monotonic(now), drift(0), estimate(now, 1_000_000, 10));
            assert_eq!(next.start, Monotonic(now));
            assert_eq!(check(next, now + 1, 0).0, 999_990);
        }

        /// A node with no real-time clock starts an hour behind, with a bound wide
        /// enough to hold the new estimate.
        #[test]
        fn steps_when_the_estimates_overlap() {
            let hour = Span::HOUR.nanos();
            let slew = Slew::new(estimate(0, 0, 2 * hour));
            let target = estimate(SECOND_NS, hour, 1_000_000);
            let next = slew.toward(Monotonic(SECOND_NS), drift(0), target);
            assert_eq!(next.from, Span::from_nanos(hour - 1_000_000));
        }

        #[test]
        fn never_steps_back() {
            let hour = Span::HOUR.nanos();
            let slew = near_zero(-hour, 0);
            assert_eq!(slew.from, Span::ZERO);
            assert_eq!(check(slew, SECOND_NS, 0), (0, hour + 10));
        }

        /// The target is from time zero, so at one second its error has grown by
        /// 1000 ns.
        #[test]
        fn counts_the_growth_of_the_target_to_now() {
            let slew = Slew::new(estimate(0, 0, 100));
            let next = |offset| {
                let target = estimate(0, offset, 10);
                slew.toward(Monotonic(SECOND_NS), drift(1_000), target).from
            };
            assert_eq!(next(501_010), Span::ZERO);
            assert_eq!(next(501_011), Span::from_nanos(500_001));
        }

        /// Mid-slew, the offset served at two seconds is -500 us.
        #[test]
        fn compares_the_offset_served_at_now() {
            let now = 2 * SECOND_NS;
            let slew = from_zero(-1_000_000);
            let next = |offset| {
                slew.toward(Monotonic(now), drift(0), estimate(now, offset, 0))
                    .from
            };
            assert_eq!(next(0), Span::from_nanos(-500_000));
            assert_eq!(next(1), Span::from_nanos(1));
        }

        /// Readers see the target's error stop at 36500 days, but with its full growth
        /// the earliest offset is only 500 us ahead, so mesh time slews.
        #[test]
        fn counts_full_growth_past_36500_days() {
            let widest = MAX_ERROR.nanos();
            let slew = Slew::new(estimate(0, 0, 0));
            let next = |offset| {
                let target = estimate(0, offset, widest);
                slew.toward(Monotonic(SECOND_NS), drift(1_000), target).from
            };
            assert_eq!(next(widest + 501_000), Span::ZERO);
            assert_eq!(next(widest + 501_001), Span::from_nanos(500_001));
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

        /// The first reading where `slew`, with `ppb` of drift, reaches `at` ns, from
        /// `now`.
        fn reach(slew: Slew, now: u64, at: i64, ppb: u32) -> u64 {
            slew.reach(Monotonic(now), Stamp::from_nanos(at), drift(ppb))
                .0
        }

        #[test]
        fn reaches_a_stamp_at_once_when_the_edge_has() {
            let slew = Slew::new(estimate(0, 0, 0));
            assert_eq!(reach(slew, SECOND_NS, 1_000, 0), SECOND_NS);
            assert_eq!(reach(slew, SECOND_NS, 1_000_000_000, 0), SECOND_NS);
        }

        /// The edge is the reading plus its growth of 1 ppm, rounded up: 1000 ns at one
        /// second, and also at 1 ns before it.
        #[test]
        fn reaches_a_stamp_first_on_a_growing_error() {
            let slew = Slew::new(estimate(0, 0, 0));
            assert_eq!(reach(slew, 0, 1_000_001_000, 1_000), SECOND_NS);
            assert_eq!(reach(slew, 0, 1_000_000_999, 1_000), SECOND_NS - 1);
        }

        /// The edge of a downward slew is the reading plus the served offset twice,
        /// less the target's, so it moves 2 ns back at each tick. The first reading
        /// that reaches `at` is before a tick that moves the edge back below it.
        #[test]
        fn reaches_a_stamp_first_before_a_tick_moves_the_edge_back() {
            let slew = from_zero(-1_000_000);
            let edge = 1_001_000_000;
            assert_eq!(reach(slew, SECOND_NS, edge + 1_999, 0), SECOND_NS + 1_999);
            let after = SECOND_NS + 2_000;
            assert_eq!(check(slew, after, 0), (-1, 999_999));
            assert_eq!(reach(slew, after, edge + 1_999, 0), after + 1);
        }

        /// At the 36500-day cap, the edge is the reading, the served offset, and the
        /// cap, so an upward slew moves it 500 ppm faster than the reading, also with
        /// no drift.
        #[test]
        fn reaches_a_stamp_first_on_an_unknown_error() {
            let target = Measurement::unknown(Monotonic(0), Span::SECOND);
            let slew = Slew {
                start: Monotonic(0),
                from: Span::ZERO,
                target,
            };
            assert_eq!(check(slew, 1_999_001, 0), (999, MAX_ERROR.nanos()));
            let at = MAX_ERROR.nanos() + 2_000_000;
            assert_eq!(reach(slew, 0, at, 0), 1_999_001);
        }

        /// The edge at the last reading is at the end of a stamp's range, also for
        /// the lowest offset.
        #[test]
        fn reaches_the_last_stamp_at_the_last_reading() {
            let slew = Slew::new(estimate(0, i64::MIN, 0));
            assert_eq!(reach(slew, 0, i64::MAX, 0), u64::MAX);
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
        use crate::world::{
            ERROR_NS, SLEW_OFFSET_NS, TIME_NS, slew, target, truechimer, world,
        };

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

        /// A slew, three readings, a drift, and a new target at the middle reading.
        /// Half the targets have an earliest offset there within 2 ns of 500 us past
        /// the served offset, where a step starts.
        fn retarget() -> impl Strategy<Value = (Slew, [u64; 3], Drift, Measurement)> {
            (slew(), readings(), 0..=100_000_000_u32).prop_flat_map(|(s, r, ppb)| {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let now = Monotonic(r[1]);
                let edge = s.at(now, drift).offset().nanos() + STEP_GAP.nanos();
                let near = (0..TIME_NS, 0..ERROR_NS, -2..=2_i64).prop_map(
                    move |(at, error, gap)| {
                        let grown = estimate(at, 0, error).error_at(now, drift);
                        estimate(at, edge + grown.nanos() + gap, error)
                    },
                );
                (Just(s), Just(r), Just(drift), prop_oneof![target(), near])
            })
        }

        proptest! {
            #[test]
            fn mesh_time_never_goes_back(
                (s, [early, switch, late], drift, target) in retarget(),
            ) {
                let next = s.toward(Monotonic(switch), drift, target);
                let before = mesh(s, early, drift);
                prop_assert!(before <= mesh(s, late, drift));
                prop_assert!(before <= mesh(next, late, drift));
                let at_switch = |slew: Slew| mesh(slew, switch, drift);
                prop_assert!(at_switch(s) <= at_switch(next));
            }

            /// Errors stay far below 36500 days, where readers see the edge that
            /// `toward` compares.
            #[test]
            fn steps_only_past_a_gap_of_500_microseconds(
                (s, [_, switch, _], drift, target) in retarget(),
            ) {
                let now = Monotonic(switch);
                let served = s.at(now, drift).offset().nanos();
                let error = target.error_at(now, drift).nanos();
                let earliest = target.offset().nanos() - error;
                let step = earliest - served > STEP_GAP.nanos();
                let expected = Span::from_nanos(if step { earliest } else { served });
                let next = s.toward(now, drift, target);
                prop_assert_eq!(next.at(now, drift).offset(), expected);
            }

            #[test]
            fn holds_the_truth_when_the_target_does(
                (w, target) in world().prop_flat_map(|w| (Just(w), truechimer(w))),
                start in 0..TIME_NS,
                from in -SLEW_OFFSET_NS..SLEW_OFFSET_NS,
                now in 0..TIME_NS,
            ) {
                let (start, from) = (Monotonic(start), Span::from_nanos(from));
                let slew = Slew { start, from, target };
                let m = slew.at(Monotonic(now), w.drift);
                prop_assert!(w.holds_truth_at(m, now), "{m:?} misses {}", w.truth(now));
            }

            /// The target is from no later than the first reading, as an estimate from
            /// `combine` is, so its earliest offset only falls and no later call steps.
            #[test]
            fn slews_as_one_however_often_it_retargets(
                (s, [_, first, _], drift, target) in retarget(),
                later in proptest::collection::vec(0..TIME_NS, 1..20),
            ) {
                let at = Monotonic(target.at().0.min(first));
                let target = Measurement::new(at, target.offset(), target.error());
                let target = target.expect("valid");
                let mut later: Vec<_> =
                    later.into_iter().map(|t| t.max(first)).collect();
                later.sort_unstable();
                let once = s.toward(Monotonic(first), drift, target);
                let often = later
                    .iter()
                    .fold(once, |slew, &t| slew.toward(Monotonic(t), drift, target));
                let last = *later.last().expect("at least one");
                for t in [last, last + 1, last + 1_999, last + 2_000, last + TIME_NS] {
                    let now = Monotonic(t);
                    prop_assert_eq!(often.at(now, drift), once.at(now, drift));
                }
            }

            #[test]
            fn moves_at_most_500_ppm(s in slew(), [early, _, late] in readings()) {
                let drift = Drift::from_ppb(0).expect("valid");
                let served = |t: u64| mesh(s, t, drift) - i128::from(t);
                let moved = (served(late) - served(early)).abs();
                prop_assert!(moved <= i128::from((late - early).div_ceil(2_000)));
            }

            /// The target's offset is held to the step edge, so `toward` slews.
            #[test]
            fn moves_at_most_500_ppm_across_a_retarget(
                (s, [early, switch, late], drift, target) in retarget(),
            ) {
                let now = Monotonic(switch);
                let served = s.at(now, drift).offset().nanos();
                let error = target.error_at(now, drift).nanos();
                let edge = served + STEP_GAP.nanos() + error;
                let offset = Span::from_nanos(target.offset().nanos().min(edge));
                let target = Measurement::new(target.at(), offset, target.error());
                let next = s.toward(now, drift, target.expect("valid"));
                let served = |slew: Slew, t| mesh(slew, t, drift) - i128::from(t);
                let moved = (served(next, late) - served(s, early)).abs();
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

            /// The edge reaches `at` at the result, and at no reading before it: at
            /// `now`, at the reading before it, and at readings between. Half the
            /// stamps are within 4 us of the edge at `now`.
            #[test]
            fn reaches_a_stamp_first_at_its_reading(
                s in slew(),
                now in 0..TIME_NS,
                ppb in prop_oneof![Just(200_000_u32), 0..=100_000_000_u32],
                ahead in prop_oneof![-4_000..4_000_i64, 0..1_i64 << 44],
                between in proptest::collection::vec(any::<u64>(), 8),
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let edge = |t: u64| s.at(Monotonic(t), drift).interval().latest;
                let at = Stamp::from_nanos(edge(now).nanos() + ahead);
                let first = s.reach(Monotonic(now), at, drift).0;
                prop_assert!(edge(first) >= at, "{first} misses {at:?}");
                if first > now {
                    let span = first - now;
                    let mut before: Vec<_> =
                        between.iter().map(|t| now + t % span).collect();
                    before.extend([now, first - 1]);
                    for t in before {
                        prop_assert!(edge(t) < at, "{t} reaches {at:?} before {first}");
                    }
                }
            }

            #[test]
            fn never_panics_at_any_input(
                start in any::<u64>(),
                from in any::<i64>(),
                target in (any::<u64>(), any::<i64>(), 0..=MAX_ERROR.nanos()),
                now in any::<[u64; 2]>(),
                ppb in 0..=100_000_000_u32,
                stamp in any::<i64>(),
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let (at, offset, error) = target;
                let target = estimate(at, offset, error);
                let (start, from) = (Monotonic(start), Span::from_nanos(from));
                let s = Slew { start, from, target };
                let next = s.toward(Monotonic(now[0]), drift, target);
                let first = s.reach(Monotonic(now[1]), Stamp::from_nanos(stamp), drift);
                prop_assert!(first >= Monotonic(now[1]));
                for slew in [s, next] {
                    let m = slew.at(Monotonic(now[1]), drift);
                    prop_assert_eq!(m.at(), Monotonic(now[1]));
                    prop_assert!(m.error() <= MAX_ERROR);
                }
            }
        }
    }
}
