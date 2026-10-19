use proptest::prelude::*;
use types::time::{Interval, Monotonic, Span, Stamp};

use super::{Fit, Fitted};

/// 2^63: the device time at which an offset reaches `i64::MIN` ns for mesh time zero.
const HALF: u64 = 1 << 63;

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

#[test]
fn a_between_reading_back_in_device_time_restarts_the_fit() {
    let mut fit = fitted();
    assert_eq!(
        fit.between(Monotonic(9_000), at(9_950), at(10_050)),
        Fitted::Restarted
    );
    assert_eq!(fit.mesh(Monotonic(9_000)), Some(within(9_950, 10_050)));
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

#[test]
fn a_new_before_reading_at_the_low_edge_is_kept() {
    let mut fit = fitted();
    assert_eq!(fit.before(Monotonic(10_000), at(10_900)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 10_900)));
}

/// At device time 5,000 ns, the fit of [`fitted`] is from 5,899 to 6,101 ns.
#[test]
fn an_old_before_reading_that_agrees_adds_nothing() {
    for arrived in [5_899, 6_000] {
        let mut fit = fitted();
        assert_eq!(fit.before(Monotonic(5_000), at(arrived)), Fitted::Kept);
        assert_eq!(fit.mesh(Monotonic(10_000)), Some(within(10_900, 11_100)));
    }
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
    assert_eq!(fit.before(Monotonic(5_000), at(i64::MIN)), Fitted::Kept);
}

/// An offset of `i64::MIN` ns fits a span, so the reading bounds the clock.
#[test]
fn a_low_edge_at_the_lowest_span_is_kept() {
    let mut fit = Fit::default();
    assert_eq!(fit.between(Monotonic(HALF), at(0), at(0)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(HALF)), Some(within(0, 0)));
}

/// A low edge under `i64::MIN` ns is left out: the lowest span would claim that mesh
/// time is at least 0 ns, past what the reading knows.
#[test]
fn a_low_edge_under_the_lowest_span_is_left_out() {
    let mut fit = Fit::default();
    assert_eq!(fit.between(Monotonic(HALF), at(-1), at(0)), Fitted::Kept);
    assert_eq!(fit.mesh(Monotonic(HALF)), None);
    assert_eq!(
        fit.between(Monotonic(u64::MAX), at(i64::MIN), at(i64::MAX)),
        Fitted::Kept
    );
    assert_eq!(fit.mesh(Monotonic(u64::MAX)), None);
}

/// A high edge under `i64::MIN` ns becomes the lowest span, which claims less. The
/// reading still counts as the newest, so a reading before it goes back.
#[test]
fn a_high_edge_under_the_lowest_span_becomes_the_lowest_span() {
    let mut fit = Fit::default();
    assert_eq!(fit.before(Monotonic(HALF + 100), at(5)), Fitted::Kept);
    assert_eq!(
        fit.between(Monotonic(HALF), at(0), at(0)),
        Fitted::Restarted
    );
    assert_eq!(fit.mesh(Monotonic(HALF)), Some(within(0, 0)));
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
    /// The timestamp of a value `back` ms before the newest device time, arrived
    /// `late` ns after the true mesh time.
    Old { back: u64, late: i64 },
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
        (0..2_000u64, 0..10_000_000i64)
            .prop_map(|(back, late)| Step::Old { back, late }),
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
                Step::Old { back, late } => {
                    let old = newest.saturating_sub(back);
                    let arrived = true_mesh(old) + Span::from_nanos(late);
                    (old, fit.before(device(old), arrived))
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
}
