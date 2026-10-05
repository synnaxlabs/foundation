use types::time::{Interval, Monotonic, Span, Stamp};

use crate::measurement::saturated;
use crate::{Drift, Measurement};

/// The fastest the served offset moves, in parts per million of local time.
const RATE_PPM: u64 = 500;

/// Mesh time that moves toward a target estimate at no more than 500 ppm, so it never
/// goes back. The part of the target not yet applied goes into the error, so the
/// interval holds the true time while the slew runs. Every value is valid.
///
/// ```
/// use estimate::{Drift, Measurement, Slew};
/// use types::time::{Monotonic, Span, Stamp};
///
/// let second = Monotonic(1_000_000_000);
/// let ahead = Measurement::new(second, Span::MILLISECOND, Span::ZERO)?;
/// let slew = Slew::new(Measurement::new(second, Span::ZERO, Span::ZERO)?);
/// let slew = slew.toward(second, ahead);
/// let mesh = slew.at(Monotonic(2_000_000_000), Drift::from_ppb(0)?);
/// assert_eq!(mesh.earliest, Stamp::from_nanos(2_000_000_000));
/// assert_eq!(mesh.latest, Stamp::from_nanos(2_001_000_000));
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

    /// Moves toward `target` from the offset served at `now`. The midpoint of mesh time
    /// from the result at or after `now` is never earlier than from `self` at or before
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

    /// Mesh time at `now`, for a local clock that drifts from mesh time by at most
    /// `drift`. The interval holds the true time when `target` holds the true offset.
    /// Its midpoint never goes back as `now` grows. The edges saturate at the range of
    /// a `Stamp`.
    #[must_use]
    pub fn at(self, now: Monotonic, drift: Drift) -> Interval {
        let served = self.served(now);
        let gap = self.target.offset().nanos().abs_diff(served);
        let error = self.target.error_at(now, drift).nanos();
        let half = i128::from(error) + i128::from(gap);
        let mid = i128::from(now.0) + i128::from(served);
        let stamp = |nanos: i128| Stamp::from_nanos(saturated(nanos).nanos());
        Interval {
            earliest: stamp(mid - half),
            latest: stamp(mid + half),
        }
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
    use types::time::{Interval, Monotonic, Span, Stamp};

    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Measurement, Slew};

    const SECOND_NS: u64 = 1_000_000_000;

    fn estimate(at: u64, offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    }

    /// Mesh time at `now` minus `now`, as `(earliest, latest)`, with `ppb` of drift.
    fn offsets(slew: Slew, now: u64, ppb: u32) -> (i128, i128) {
        let drift = Drift::from_ppb(ppb).expect("valid");
        let Interval { earliest, latest } = slew.at(Monotonic(now), drift);
        let from_now = |s: Stamp| i128::from(s.nanos()) - i128::from(now);
        (from_now(earliest), from_now(latest))
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
            assert_eq!(offsets(slew, 1_000, 0), (4_900, 5_100));
            let start = (slew.start, slew.from);
            assert_eq!(start, (Monotonic(1_000), Span::from_nanos(5_000)));
        }

        #[test]
        fn keeps_mesh_time_when_the_target_changes() {
            let before = Slew::new(estimate(0, 0, 0));
            assert_eq!(offsets(before, SECOND_NS, 0), (0, 0));
            let after = from_zero(1_000_000);
            assert_eq!(offsets(after, SECOND_NS, 0), (-1_000_000, 1_000_000));
            assert_eq!(
                (after.start, after.from),
                (Monotonic(SECOND_NS), Span::ZERO)
            );
        }

        #[test]
        fn continues_from_the_offset_served_mid_slew() {
            let now = Monotonic(2 * SECOND_NS);
            let slew = from_zero(1_000_000).toward(now, estimate(0, 0, 0));
            assert_eq!(slew.from, Span::from_nanos(500_000));
            assert_eq!(offsets(slew, 2 * SECOND_NS, 0), (0, 1_000_000));
        }

        #[test]
        fn moves_500_microseconds_in_a_second() {
            let forward = from_zero(1_000_000);
            assert_eq!(offsets(forward, 2 * SECOND_NS, 0), (0, 1_000_000));
            let back = from_zero(-1_000_000);
            assert_eq!(offsets(back, 2 * SECOND_NS, 0), (-1_000_000, 0));
        }

        #[test]
        fn rounds_the_served_offset_down() {
            let slew = from_zero(1_000_000);
            let still = offsets(slew, SECOND_NS + 1_999, 0);
            assert_eq!(still, (-1_000_000, 1_000_000));
            let moved = offsets(slew, SECOND_NS + 2_000, 0);
            assert_eq!(moved, (-999_998, 1_000_000));
        }

        #[test]
        fn stops_at_the_target() {
            let slew = from_zero(1_000_000);
            assert_eq!(offsets(slew, 3 * SECOND_NS, 0), (1_000_000, 1_000_000));
            assert_eq!(offsets(slew, 100 * SECOND_NS, 0), (1_000_000, 1_000_000));
        }

        #[test]
        fn adds_the_part_not_applied_to_the_drifted_error() {
            let now = Monotonic(SECOND_NS);
            let target = estimate(SECOND_NS, 1_000_000, 10);
            let slew = Slew::new(estimate(0, 0, 0)).toward(now, target);
            let half = 500_000 + 10 + 1_000;
            let bounds = (500_000 - half, 500_000 + half);
            assert_eq!(offsets(slew, 2 * SECOND_NS, 1_000), bounds);
        }

        #[test]
        fn serves_from_at_and_before_the_start() {
            let slew = Slew {
                start: Monotonic(SECOND_NS),
                from: Span::from_nanos(7),
                target: estimate(SECOND_NS, 1_000_000, 0),
            };
            assert_eq!(offsets(slew, 0, 0), (-999_986, 1_000_000));
            assert_eq!(offsets(slew, SECOND_NS, 0), (-999_986, 1_000_000));
        }

        #[test]
        fn saturates_at_the_range_of_a_stamp() {
            let (widest, drift) =
                (MAX_ERROR.nanos(), Drift::from_ppb(0).expect("valid"));
            let high = Slew::new(estimate(u64::MAX, i64::MAX, widest));
            let mesh = high.at(Monotonic(u64::MAX), drift);
            let max = Stamp::from_nanos(i64::MAX);
            assert_eq!(
                mesh,
                Interval {
                    earliest: max,
                    latest: max
                }
            );
            let low = Slew::new(estimate(0, i64::MIN, widest));
            let mesh = low.at(Monotonic(0), drift);
            let earliest = Stamp::from_nanos(i64::MIN);
            let latest = Stamp::from_nanos(i64::MIN + widest);
            assert_eq!(mesh, Interval { earliest, latest });
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::world::{ERROR_NS, TIME_NS, truechimer, world};

        const OFFSET_NS: i64 = 1 << 50;

        /// A slew with times, offsets, and errors that never saturate a `Stamp`.
        fn slew() -> impl Strategy<Value = Slew> {
            let target = (0..TIME_NS, -OFFSET_NS..OFFSET_NS, 0..ERROR_NS);
            (0..TIME_NS, -OFFSET_NS..OFFSET_NS, target).prop_map(
                |(start, from, (at, offset, error))| Slew {
                    start: Monotonic(start),
                    from: Span::from_nanos(from),
                    target: estimate(at, offset, error),
                },
            )
        }

        /// Twice the midpoint of mesh time at `now`, exact for an interval that does
        /// not saturate.
        fn twice_mid(slew: Slew, now: u64, drift: Drift) -> i128 {
            let Interval { earliest, latest } = slew.at(Monotonic(now), drift);
            i128::from(earliest.nanos()) + i128::from(latest.nanos())
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
            fn midpoint_never_goes_back(
                s in slew(),
                [early, switch, late] in readings(),
                target in (0..TIME_NS, -OFFSET_NS..OFFSET_NS, 0..ERROR_NS),
                ppb in 0..=100_000_000_u32,
            ) {
                let drift = Drift::from_ppb(ppb).expect("valid");
                let (at, offset, error) = target;
                let next = s.toward(Monotonic(switch), estimate(at, offset, error));
                let before = twice_mid(s, early, drift);
                prop_assert!(before <= twice_mid(s, late, drift));
                prop_assert!(before <= twice_mid(next, late, drift));
                let at_switch = |slew: Slew| twice_mid(slew, switch, drift);
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
                let Interval { earliest, latest } = slew.at(Monotonic(now), w.drift);
                let truth = i128::from(now) + w.truth(now);
                prop_assert!(i128::from(earliest.nanos()) <= truth);
                prop_assert!(truth <= i128::from(latest.nanos()));
            }

            #[test]
            fn moves_at_most_500_ppm(s in slew(), [early, _, late] in readings()) {
                let drift = Drift::from_ppb(0).expect("valid");
                let served = |t: u64| twice_mid(s, t, drift) - 2 * i128::from(t);
                let moved = (served(late) - served(early)).abs();
                prop_assert!(moved <= 2 * i128::from((late - early).div_ceil(2_000)));
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
                    let mesh = slew.at(Monotonic(now[1]), drift);
                    prop_assert!(mesh.earliest <= mesh.latest, "{mesh:?}");
                }
            }
        }
    }
}
