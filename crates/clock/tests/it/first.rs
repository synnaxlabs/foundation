use std::iter;

use clock::{Clock, Reader, Status};
use estimate::Measurement;
use estimate::combine::Error;
use estimate::discipline::Cause;
use proptest::prelude::*;
use sim::node::Node;
use types::time::{Interval, Monotonic, Span};

use crate::common::{UNKNOWN, ms, node};

const HALF_HOUR: Span = Span::from_nanos(30 * Span::MINUTE.nanos());

/// `a` plus `b`.
fn plus(a: Span, b: Span) -> Span {
    Span::from_nanos(a.nanos() + b.nanos())
}

/// A measurement at the node's monotonic reading now.
fn measure(node: &Node, offset: Span, error: Span) -> Measurement {
    let at = node.clock().now();
    Measurement::new(at, offset, error).expect("at most 36500 days")
}

/// A reading of the node's monotonic clock, taken before a read of `reader` that
/// gives no time.
fn unsynced(node: &Node, reader: &Reader) -> Monotonic {
    let reading = node.clock().now();
    assert_eq!(reader.now(), None, "no time yet");
    reading
}

/// Mesh time at `reading` with `offset` and `error`.
fn at(reading: Monotonic, offset: Span, error: Span) -> Option<Interval> {
    Measurement::new(reading, offset, error).map(Measurement::interval)
}

/// Why the clock holds over, or `None` when it does not.
fn holdover(reader: &Reader) -> Option<Cause> {
    match reader.status() {
        Status::Holdover(_, cause) => Some(cause),
        Status::Unsynced(_) | Status::Synced(_) => None,
    }
}

/// The midpoint of `interval`, in nanoseconds.
fn center(interval: Interval) -> i64 {
    interval.earliest.nanos().midpoint(interval.latest.nanos())
}

#[test]
fn gives_nothing_before_the_first_estimate() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let reading = unsynced(&node, &reader);
    assert_eq!(reader.first(reading), None);
    let [a, _] = [clock.add(), clock.add()];
    clock.push(a, measure(&node, Span::HOUR, ms(2)));
    assert_eq!(reader.now(), None);
    assert_eq!(reader.first(reading), None);
}

#[test]
fn grows_the_error_of_the_first_estimate_back_to_each_earlier_reading() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let start = unsynced(&node, &reader);
    sim.run_for(HALF_HOUR).expect("the run ends");
    let half = unsynced(&node, &reader);
    sim.run_for(HALF_HOUR).expect("the run ends");
    clock.push(source, measure(&node, Span::HOUR, ms(2)));
    // Drift adds 720 ms in an hour, and 360 ms in half an hour.
    assert_eq!(reader.first(start), at(start, Span::HOUR, ms(722)));
    assert_eq!(reader.first(half), at(half, Span::HOUR, ms(362)));
}

#[test]
fn grows_the_error_of_the_first_estimate_to_a_later_reading() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    clock.push(source, measure(&node, Span::HOUR, ms(2)));
    sim.run_for(Span::HOUR).expect("the run ends");
    let reading = node.clock().now();
    assert_eq!(reader.first(reading), at(reading, Span::HOUR, ms(722)));
}

#[test]
fn keeps_the_first_estimate_after_a_step_a_holdover_and_no_sources() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let reading = unsynced(&node, &reader);
    sim.run_for(Span::SECOND).expect("the run ends");
    clock.push(source, measure(&node, Span::HOUR, ms(2)));
    // A second of drift adds 200 us.
    let first = at(reading, Span::HOUR, Span::from_nanos(2_200_000));
    assert_eq!(reader.first(reading), first);
    sim.run_for(Span::SECOND).expect("the run ends");
    clock.push(
        source,
        measure(&node, plus(Span::HOUR, Span::SECOND), ms(1)),
    );
    let stepped = at(node.clock().now(), plus(Span::HOUR, ms(999)), ms(2));
    assert_eq!(reader.now(), stepped);
    assert_eq!(reader.first(reading), first);
    let other = clock.add();
    let alone = Error::NoMajority {
        sources: 2,
        agreeing: 1,
        empty: 1,
    };
    assert_eq!(holdover(&reader), Some(Cause::NoEstimate(alone)));
    assert_eq!(reader.first(reading), first);
    clock.remove(source);
    clock.remove(other);
    assert_eq!(holdover(&reader), Some(Cause::NoEstimate(Error::NoSources)));
    assert_eq!(reader.first(reading), first);
}

#[test]
fn an_unknown_first_estimate_gives_unknown_time_at_the_reading() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let reading = unsynced(&node, &reader);
    sim.run_for(Span::HOUR).expect("the run ends");
    clock.push(source, Measurement::unknown(node.clock().now(), Span::HOUR));
    let unknown = Measurement::unknown(reading, Span::HOUR);
    assert_eq!(unknown.error(), UNKNOWN);
    assert_eq!(reader.first(reading), Some(unknown.interval()));
}

/// Mesh time near each end of the range where an interval with an error near 100
/// years fits a stamp. The clock moves from a source with such an estimate to one
/// with a narrow estimate, and the other way.
#[test]
fn stamps_an_earlier_reading_before_mesh_time_from_1777_to_2162() {
    let near = Span::from_nanos(UNKNOWN.nanos() - Span::DAY.nanos());
    // About 1780 and 2160.
    for offset in [-6_000_000_000_000_000_000, 6_000_000_000_000_000_000] {
        let offset = Span::from_nanos(offset);
        for wide in [UNKNOWN, near] {
            for [before, after] in [[wide, ms(1)], [ms(1), wide]] {
                let (mut sim, node) = node();
                let (mut clock, reader) = Clock::new(node.clock());
                let reading = unsynced(&node, &reader);
                let old = clock.add();
                sim.run_for(Span::SECOND).expect("the run ends");
                clock.push(old, measure(&node, offset, before));
                let mut centers = vec![center(reader.now().expect("synced"))];
                let new = clock.add();
                clock.push(new, measure(&node, offset, after));
                clock.remove(old);
                sim.run_for(Span::SECOND).expect("the run ends");
                clock.push(new, measure(&node, offset, after));
                centers.push(center(reader.now().expect("synced")));
                let first = center(reader.first(reading).expect("synced"));
                assert!(first <= centers[0], "{first} {centers:?}");
                assert!(centers.is_sorted(), "{centers:?}");
            }
        }
    }
}

fn gap() -> impl Strategy<Value = Span> {
    (0..=Span::HOUR.nanos()).prop_map(Span::from_nanos)
}

fn estimate() -> impl Strategy<Value = (Span, Span, Span)> {
    let offset = (-Span::DAY.nanos()..=Span::DAY.nanos()).prop_map(Span::from_nanos);
    let error = prop_oneof![
        (0..=Span::SECOND.nanos()).prop_map(Span::from_nanos),
        Just(UNKNOWN),
    ];
    (gap(), offset, error)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Readings taken before the first estimate keep their order, and stay before
    /// each mesh time after it, through steps, slews, and unknown estimates.
    #[test]
    fn stamps_earlier_readings_in_order_and_before_mesh_time(
        gaps in prop::collection::vec(gap(), 1..4),
        first in estimate(),
        later in prop::collection::vec(estimate(), 0..6),
    ) {
        let (mut sim, node) = node();
        let (mut clock, reader) = Clock::new(node.clock());
        let source = clock.add();
        let mut readings = Vec::new();
        for gap in gaps {
            sim.run_for(gap).expect("the run ends");
            readings.push(unsynced(&node, &reader));
        }
        let mut stamps = None;
        for (gap, offset, error) in iter::once(first).chain(later) {
            sim.run_for(gap).expect("the run ends");
            clock.push(source, measure(&node, offset, error));
            let now = center(reader.now().expect("synced"));
            let firsts: Vec<_> = readings
                .iter()
                .map(|&reading| reader.first(reading).expect("synced"))
                .collect();
            let centers: Vec<_> = firsts.iter().copied().map(center).collect();
            prop_assert!(centers.is_sorted(), "{centers:?}");
            prop_assert!(centers.iter().all(|&c| c <= now), "{centers:?} {now}");
            let kept = stamps.get_or_insert_with(|| firsts.clone());
            prop_assert_eq!(&*kept, &firsts);
        }
    }
}
