use proptest::prelude::*;
use types::time::{Interval, Monotonic, Span, Stamp};

use super::{Fit, Fitted};

const MIN: i64 = i64::MIN;
const MAX: i64 = i64::MAX;

fn at(ns: i64) -> Stamp {
    Stamp::from_nanos(ns)
}

fn within(earliest: i64, latest: i64) -> Interval {
    Interval {
        earliest: at(earliest),
        latest: at(latest),
    }
}

/// A fit with one `between` reading: at device time 10,000 ns, the offset is from
/// 900 to 1,100 ns.
fn fitted() -> Fit {
    let mut fit = Fit::default();
    assert_eq!(
        fit.between(Monotonic(10_000), at(10_900), at(11_100)),
        Fitted::Kept
    );
    fit
}

#[test]
fn an_empty_fit_bounds_no_device_time() {
    assert_eq!(Fit::default().mesh(Monotonic(0)), None);
}

#[test]
fn a_between_reading_bounds_the_clock() {
    assert_eq!(
        fitted().mesh(Monotonic(10_000)),
        Some(within(10_900, 11_100))
    );
}

#[test]
fn two_between_readings_narrow_the_fit() {
    let mut fit = fitted();
    assert_eq!(
        fit.between(Monotonic(10_000), at(11_000), at(11_200)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(11_000, 11_100)));
}

#[test]
fn the_fit_widens_by_200_ppm_away_from_its_readings() {
    let fit = fitted();
    assert_eq!(
        fit.mesh(Monotonic(5_010_000)),
        Some(within(5_009_900, 5_012_100))
    );
    assert_eq!(fit.mesh(Monotonic(5_000)), Some(within(5_899, 6_101)));
}

#[test]
fn a_reversed_between_reading_is_taken_in_order() {
    let mut fit = Fit::default();
    assert_eq!(
        fit.between(Monotonic(10_000), at(11_100), at(10_900)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 11_100)));
}

#[test]
fn a_before_reading_alone_bounds_no_device_time() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(10_000), at(11_050)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(10_000)), None);
}

#[test]
fn a_before_reading_narrows_the_top_of_a_between_reading() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(10_000), at(11_050)), Fitted::Kept);
    assert_eq!(
        fit.between(Monotonic(10_000), at(10_900), at(11_100)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 11_050)));
}

/// At device time 10,000 ns, 200 ppm widens the reading at 9,000 ns by 0.2 ns, which
/// rounds outward to 1 ns.
#[test]
fn an_old_between_reading_that_agrees_narrows_the_fit() {
    let mut fit = fitted();
    assert_eq!(
        fit.between(Monotonic(9_000), at(9_950), at(10_050)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_949, 11_051)));
}

#[test]
fn an_old_between_reading_that_disagrees_restarts_the_fit() {
    let mut fit = fitted();
    assert_eq!(
        fit.between(Monotonic(9_000), at(20_000), at(20_100)),
        Fitted::Restarted
    );
    assert_eq!(fit.mesh(Monotonic(9_000)), Some(within(20_000, 20_100)));
}

/// A true `between` older than a true `before` agrees with it.
#[test]
fn an_old_true_between_reading_after_a_before_reading_is_kept() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(10_000), at(11_000)), Fitted::Kept);
    assert_eq!(
        fit.between(Monotonic(9_000), at(9_950), at(10_050)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_948, 11_000)));
}

#[test]
fn a_disjoint_between_reading_restarts_the_fit() {
    let mut fit = fitted();
    assert_eq!(
        fit.between(Monotonic(10_000), at(20_000), at(20_100)),
        Fitted::Restarted
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(20_000, 20_100)));
}

#[test]
fn a_new_disjoint_before_reading_restarts_the_fit() {
    let mut fit = fitted();
    assert_eq!(fit.before(Monotonic(10_000), at(10_899)), Fitted::Restarted);
    assert_eq!(fit.mesh(Monotonic(10_000)), None);
    assert_eq!(
        fit.between(Monotonic(10_000), at(10_799), at(11_000)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_799, 10_899)));
}

/// The fit is 201 ns wide, so its interval starts 1 ns under its low edge.
#[test]
fn a_new_before_reading_under_the_low_edge_of_an_odd_fit_restarts_the_fit() {
    let mut fit = Fit::default();
    assert_eq!(
        fit.between(Monotonic(10_000), at(10_900), at(11_101)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_899, 11_101)));
    assert_eq!(fit.before(Monotonic(10_000), at(10_899)), Fitted::Restarted);
}

#[test]
fn a_new_before_reading_at_the_low_edge_is_kept() {
    let mut fit = fitted();
    assert_eq!(fit.before(Monotonic(10_000), at(10_900)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 10_900)));
}

/// At device time 5,000 ns, the fit of [`fitted`] is from 5,899 to 6,101 ns. At
/// 10,000 ns, 200 ppm widens a reading at 5,000 ns by 1 ns.
#[test]
fn an_old_before_reading_that_agrees_narrows_the_fit() {
    let mut fit = fitted();
    assert_eq!(fit.before(Monotonic(5_000), at(6_000)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_899, 11_001)));
    assert_eq!(fit.before(Monotonic(5_000), at(5_899)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 10_900)));
}

#[test]
fn an_old_before_reading_that_disagrees_restarts_the_fit() {
    let mut fit = fitted();
    assert_eq!(fit.before(Monotonic(5_000), at(5_898)), Fitted::Restarted);
    assert_eq!(fit.mesh(Monotonic(5_000)), None);
    assert_eq!(
        fit.between(Monotonic(5_000), at(5_800), at(6_000)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(5_000)), Some(within(5_800, 5_898)));
}

#[test]
fn an_old_before_reading_of_a_fit_with_no_low_edge_is_kept() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(10_000), at(10_000)), Fitted::Kept);
    assert_eq!(fit.before(Monotonic(5_000), at(MIN)), Fitted::Kept);
}

/// The fit holds offsets from that of its first reading, `i64::MAX` ns here. A low
/// edge of `i64::MIN` ns from it is kept, and the next reading narrows the fit to it.
#[test]
fn a_low_edge_at_the_lowest_span_is_kept() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MAX)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(-1), at(MAX)), Fitted::Kept);
    assert_eq!(fit.before(Monotonic(0), at(-1)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(-1, -1)));
}

/// A low edge under `i64::MIN` ns from the first offset is left out: the lowest span
/// would claim more than the reading knows.
#[test]
fn a_low_edge_under_the_lowest_span_is_left_out() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MAX)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(-2), at(MAX)), Fitted::Kept);
    assert_eq!(fit.before(Monotonic(0), at(-2)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), None);
}

/// A `between` reading whose low edge is left out still narrows the fit from above.
#[test]
fn a_between_reading_with_no_low_edge_keeps_its_high_edge() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MAX)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(-2), at(-1)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(-1), at(0)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(-1, -1)));
}

/// A high edge under `i64::MIN` ns from the first offset becomes the lowest span,
/// which claims less: a later `-1` agrees with a `before` of `-2`.
#[test]
fn a_high_edge_under_the_lowest_span_becomes_the_lowest_span() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MAX)), Fitted::Kept);
    assert_eq!(fit.before(Monotonic(0), at(-2)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(-1), at(MAX)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(-1, -1)));
}

/// A low edge over `i64::MAX` ns from the first offset becomes the highest span,
/// which claims less, and still restarts a fit that it disagrees with.
#[test]
fn a_low_edge_over_the_highest_span_becomes_the_highest_span() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MIN)), Fitted::Kept);
    assert_eq!(
        fit.between(Monotonic(0), at(MAX), at(MAX)),
        Fitted::Restarted
    );
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(MAX, MAX)));
}

/// A high edge over `i64::MAX` ns from the first offset is left out: the highest span
/// would claim more than the reading knows.
#[test]
fn a_high_edge_over_the_highest_span_is_left_out() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(0), at(MIN)), Fitted::Kept);
    assert_eq!(fit.before(Monotonic(0), at(MAX)), Fitted::Kept);
    assert_eq!(fit.between(Monotonic(0), at(MIN), at(MIN)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(MIN, MIN)));
}

/// A bound past the latest stamp is `None`, and one that only ends past it stops at it.
#[test]
fn a_bound_past_the_range_of_a_stamp_is_none() {
    let mut fit = Fit::default();
    assert_eq!(fit.between(Monotonic(0), at(MAX), at(MAX)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(1)), Some(within(MAX, MAX)));
    assert_eq!(fit.mesh(Monotonic(10)), None);
    let mut fit = Fit::default();
    assert_eq!(fit.between(Monotonic(10), at(MIN), at(MIN)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(9)), Some(within(MIN, MIN)));
    assert_eq!(fit.mesh(Monotonic(0)), None);
}

/// 36,500 days in ns: the widest bound that a fit gives.
const WIDEST: i64 = 36_500 * 86_400 * 1_000_000_000;

/// A device time far from a reading of a fit almost 2 x 36,500 days wide, where 200 ppm
/// makes the bound wider than 36,500 days.
const FAR: i64 = 1_000_000_000_000_000_000;

#[test]
fn a_bound_of_36500_days_or_wider_is_none() {
    let mut fit = Fit::default();
    let width = 2 * WIDEST - 2;
    assert_eq!(fit.between(Monotonic(0), at(0), at(width)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(0)), Some(within(0, width)));
    assert_eq!(fit.mesh(Monotonic(FAR.unsigned_abs())), None);
}

#[test]
fn a_true_old_before_of_a_wide_fit_is_kept() {
    let mut fit = Fit::default();
    assert_eq!(
        fit.between(Monotonic(FAR.unsigned_abs()), at(FAR), at(FAR + 2 * WIDEST)),
        Fitted::Kept
    );
    assert_eq!(
        fit.before(Monotonic(0), at(-200_000_000_000_000 + 1)),
        Fitted::Kept
    );
}

/// A device clock, read in the order its readings arrive.
#[derive(Clone, Debug)]
enum Step {
    /// A read of the clock, `ms` later in device time, sent `early` ns before and
    /// answered `late` ns after the true mesh time, in either order.
    Between {
        ms: u64,
        early: i64,
        late: i64,
        reversed: bool,
    },
    /// The timestamp of a value `ms` later in device time, arrived `late` ns after the
    /// true mesh time.
    Before { ms: u64, late: i64 },
    /// A reading `back` ms before the newest device time: a read sent `early` ns
    /// before the true mesh time, or the timestamp of a value for `None`, which
    /// arrived `late` ns after it.
    Old {
        back: u64,
        early: Option<i64>,
        late: i64,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..1_000u64, 0..1_000_000i64, 0..1_000_000i64, any::<bool>()).prop_map(
            |(ms, early, late, reversed)| Step::Between {
                ms,
                early,
                late,
                reversed
            }
        ),
        (0..1_000u64, 0..10_000_000i64)
            .prop_map(|(ms, late)| Step::Before { ms, late }),
        (
            0..2_000u64,
            prop::option::of(0..1_000_000i64),
            0..10_000_000i64
        )
            .prop_map(|(back, early, late)| Step::Old { back, early, late }),
    ]
}

proptest! {
    /// A device clock whose offset moves by `ppm` from mesh time: `ppm` ns for each
    /// ms of device time, at most 200 ppm. No true reading restarts the fit, each
    /// device time maps to an interval that holds its true mesh time, and the fit
    /// bounds the newest device time from the first `between` reading on.
    #[test]
    fn true_readings_keep_the_fit_and_hold_the_true_time(
        start in 0..1u64 << 62,
        base in -(1i64 << 62) + (1 << 30)..1i64 << 62,
        ppm in -200..=200i64,
        steps in prop::collection::vec(step(), 1..40),
        query in 0..50_000u64,
    ) {
        let device = |ms: u64| Monotonic(start + ms * 1_000_000);
        let true_mesh = |ms: u64| {
            let ms = i64::try_from(ms).expect("a small count");
            at(base + ms * 1_000_000 + ms * ppm)
        };
        let mut fit = Fit::default();
        let mut newest = 0;
        let mut bounded = false;
        for step in steps {
            let (ms, fitted) = match step {
                Step::Between { ms, early, late, reversed } => {
                    newest += ms;
                    let truth = true_mesh(newest);
                    let (sent, answered) = (
                        truth - Span::from_nanos(early),
                        truth + Span::from_nanos(late),
                    );
                    let (sent, answered) =
                        if reversed { (answered, sent) } else { (sent, answered) };
                    bounded = true;
                    (newest, fit.between(device(newest), sent, answered))
                }
                Step::Before { ms, late } => {
                    newest += ms;
                    let arrived = true_mesh(newest) + Span::from_nanos(late);
                    (newest, fit.before(device(newest), arrived))
                }
                Step::Old { back, early, late } => {
                    let old = newest.saturating_sub(back);
                    let truth = true_mesh(old);
                    let arrived = truth + Span::from_nanos(late);
                    let fitted = match early {
                        Some(early) => {
                            bounded = true;
                            let sent = truth - Span::from_nanos(early);
                            fit.between(device(old), sent, arrived)
                        }
                        None => fit.before(device(old), arrived),
                    };
                    (old, fitted)
                }
            };
            prop_assert_eq!(fitted, Fitted::Kept);
            prop_assert_eq!(fit.mesh(device(newest)).is_some(), bounded);
            for ms in [ms, newest, query] {
                if let Some(mesh) = fit.mesh(device(ms)) {
                    let truth = true_mesh(ms);
                    prop_assert!(
                        mesh.earliest <= truth && truth <= mesh.latest,
                        "{mesh:?} does not hold {truth:?} at {ms} ms"
                    );
                }
            }
        }
    }

    /// Any reading of any device time: no reading panics, a restart holds only its
    /// reading, and a fit of true readings of one device time, with any first offset,
    /// holds the true mesh time.
    #[test]
    fn any_reading_keeps_its_edges_on_their_sides(
        device in any::<u64>(),
        mut times in prop::array::uniform5(any::<i64>()),
        readings in prop::collection::vec(
            (any::<u64>(), any::<i64>(), any::<i64>(), any::<bool>()),
            0..8,
        ),
    ) {
        times.sort_unstable();
        let [early, sent, truth, answered, late] = times.map(at);
        let device = Monotonic(device);
        let read = |fit: &mut Fit, (device, a, b, between)| {
            if between {
                fit.between(Monotonic(device), at(a), at(b))
            } else {
                fit.before(Monotonic(device), at(a))
            }
        };
        let mut fit = Fit::default();
        for reading in readings {
            if read(&mut fit, reading) == Fitted::Restarted {
                let mut alone = Fit::default();
                prop_assert_eq!(read(&mut alone, reading), Fitted::Kept);
                let device = Monotonic(reading.0);
                prop_assert_eq!(fit.mesh(device), alone.mesh(device));
            }
        }

        for first in [early, late] {
            let mut fit = Fit::default();
            prop_assert_eq!(fit.before(device, first.max(truth)), Fitted::Kept);
            prop_assert_eq!(fit.between(device, answered, sent), Fitted::Kept);
            prop_assert_eq!(fit.before(device, answered), Fitted::Kept);
            prop_assert_eq!(fit.between(device, first.min(truth), late), Fitted::Kept);
            if let Some(mesh) = fit.mesh(device) {
                prop_assert!(
                    mesh.earliest <= truth && truth <= mesh.latest,
                    "{mesh:?} does not hold {truth:?}"
                );
            }
        }
    }
}
