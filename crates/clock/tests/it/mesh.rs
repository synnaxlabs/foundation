use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use clock::{Clock, Reader, Status, Time};
use estimate::Measurement;
use estimate::combine::Error;
use estimate::discipline::Cause;
use sim::node::Node;
use types::time::{Monotonic, Span};

use crate::common::{UNKNOWN, ms, node, time};

fn us(n: i64) -> Span {
    Span::from_nanos(n * 1_000)
}

/// A measurement at the node's monotonic reading now.
fn measure(node: &Node, offset: Span, error: Span) -> Measurement {
    let at = node.clock().now();
    Measurement::new(at, offset, error).expect("at most 36500 days")
}

fn synced(node: &Node, offset: Span, error: Span) -> Status {
    Status::Synced(measure(node, offset, error))
}

fn holdover(node: &Node, offset: Span, error: Span, cause: Cause) -> Status {
    Status::Holdover(measure(node, offset, error), cause)
}

/// Mesh time that `reader` gives now, as the interval of a measurement now.
fn read(reader: &Reader) -> Option<Measurement> {
    let Time { monotonic, mesh } = reader.now();
    let interval = mesh?;
    let now = i128::from(monotonic.0);
    let (earliest, latest) = (interval.earliest.nanos(), interval.latest.nanos());
    let offset = i128::from(earliest).midpoint(i128::from(latest)) - now;
    let m = Measurement::new(
        monotonic,
        Span::from_nanos(i64::try_from(offset).expect("fits")),
        Span::from_nanos((latest - earliest) / 2),
    );
    assert_eq!(
        m.map(Measurement::interval),
        Some(interval),
        "an even interval"
    );
    m
}

#[test]
fn has_no_time_with_no_sources() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    assert_eq!(reader.now(), time(&node, None));
    let source = clock.add();
    clock.remove(source);
    assert_eq!(reader.status(), Status::Unsynced(Error::NoSources));
    assert_eq!(reader.now(), time(&node, None));
}

#[test]
fn gives_its_monotonic_reading_with_no_time() {
    let (mut sim, node) = node();
    let (_clock, reader) = Clock::new(node.clock());
    sim.run_for(Span::SECOND).expect("the run ends");
    assert_eq!(reader.now(), time(&node, None));
}

/// A monotonic clock that moves 1 ms at each read.
struct Ticking(AtomicU64);

impl env::clock::Driver for Ticking {
    fn now(&self) -> Monotonic {
        Monotonic(self.0.fetch_add(1_000_000, Ordering::Relaxed))
    }

    fn epoch(&self) -> Instant {
        unreachable!("the test only reads")
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        unreachable!("the test only reads")
    }
}

#[test]
fn gives_mesh_time_at_its_own_monotonic_reading() {
    let monotonic = env::clock::Clock::new(Ticking(AtomicU64::new(0)));
    let (mut clock, reader) = Clock::new(monotonic.clone());
    let source = clock.add();
    let m = Measurement::new(monotonic.now(), Span::HOUR, ms(2));
    clock.push(source, m.expect("at most 36500 days"));
    let mut last = Monotonic(0);
    for _ in 0..3 {
        let Time { monotonic, mesh } = reader.now();
        assert!(monotonic > last, "{monotonic:?} after {last:?}");
        let mesh = mesh.expect("synced");
        let center = mesh.earliest.nanos().midpoint(mesh.latest.nanos());
        let reading = i64::try_from(monotonic.0).expect("fits");
        assert_eq!(
            center - reading,
            Span::HOUR.nanos(),
            "{mesh:?} at {monotonic:?}"
        );
        last = monotonic;
    }
}

#[test]
fn reads_the_monotonic_clock_once_with_no_time() {
    let monotonic = env::clock::Clock::new(Ticking(AtomicU64::new(0)));
    let (_clock, reader) = Clock::new(monotonic.clone());
    let start = monotonic.now();
    for tick in 1..=3 {
        let monotonic = Monotonic(start.0 + tick * 1_000_000);
        assert_eq!(
            reader.now(),
            Time {
                monotonic,
                mesh: None
            }
        );
    }
}

#[test]
fn has_no_time_until_a_majority_agrees() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [a, b, _] = [clock.add(), clock.add(), clock.add()];
    let m = measure(&node, Span::HOUR, ms(2));
    let alone = Error::NoMajority {
        sources: 3,
        agreeing: 1,
        empty: 2,
    };
    clock.push(a, m);
    assert_eq!(reader.status(), Status::Unsynced(alone));
    assert_eq!(reader.now(), time(&node, None));
    clock.push(b, m);
    assert_eq!(reader.status(), Status::Synced(m));
    assert_eq!(reader.now(), time(&node, Some(m.interval())));
}

#[test]
fn the_status_follows_each_add_before_the_first_estimate() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    assert_eq!(reader.status(), Status::Unsynced(Error::NoSources));
    let _: [clock::source::Key; 2] = [clock.add(), clock.add()];
    let empty = Error::NoMajority {
        sources: 2,
        agreeing: 0,
        empty: 2,
    };
    assert_eq!(reader.status(), Status::Unsynced(empty));
}

#[test]
fn an_add_after_the_first_estimate_holds_over_at_once() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let first = measure(&node, Span::HOUR, ms(2));
    clock.push(source, first);
    assert_eq!(reader.status(), Status::Synced(first));
    let _: clock::source::Key = clock.add();
    let alone = Error::NoMajority {
        sources: 2,
        agreeing: 1,
        empty: 1,
    };
    let cause = Cause::NoEstimate(alone);
    assert_eq!(reader.status(), Status::Holdover(first, cause));
    assert_eq!(reader.now(), time(&node, Some(first.interval())));
}

#[test]
fn a_source_that_pushes_first_cannot_set_mesh_time() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [liar, a, b] = [clock.add(), clock.add(), clock.add()];
    clock.push(liar, measure(&node, Span::HOUR, ms(1)));
    let truth = measure(&node, Span::ZERO, ms(1));
    let split = Error::NoMajority {
        sources: 3,
        agreeing: 1,
        empty: 1,
    };
    clock.push(a, truth);
    assert_eq!(reader.status(), Status::Unsynced(split));
    assert_eq!(reader.now(), time(&node, None));
    clock.push(b, truth);
    assert_eq!(reader.status(), Status::Synced(truth));
    assert_eq!(reader.now(), time(&node, Some(truth.interval())));
}

#[test]
fn serves_the_first_measurement_at_once() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let first = measure(&node, Span::HOUR, ms(2));
    clock.push(source, first);
    assert_eq!(reader.status(), Status::Synced(first));
    assert_eq!(reader.now(), time(&node, Some(first.interval())));
}

#[test]
fn an_unknown_source_alone_gives_unknown_time() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let unknown = Measurement::unknown(node.clock().now(), Span::HOUR);
    clock.push(source, unknown);
    assert_eq!(reader.status(), Status::Synced(unknown));
    assert_eq!(unknown.error(), UNKNOWN);
    assert_eq!(reader.now(), time(&node, Some(unknown.interval())));
}

/// An unknown estimate never replaces a known one.
#[test]
fn holds_over_when_only_an_unknown_source_is_left() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [peer, os] = [clock.add(), clock.add()];
    let known = measure(&node, Span::ZERO, ms(1));
    let os_offset = Span::from_nanos(5 * Span::SECOND.nanos());
    clock.push(peer, known);
    clock.push(os, Measurement::unknown(node.clock().now(), os_offset));
    assert_eq!(reader.status(), Status::Synced(known));
    clock.remove(peer);
    let unknown = holdover(&node, Span::ZERO, ms(1), Cause::UnknownEstimate);
    assert_eq!(reader.status(), unknown);
    sim.run_for(Span::HOUR).expect("the run ends");
    clock.push(os, Measurement::unknown(node.clock().now(), os_offset));
    // An hour of drift adds 720 ms.
    let grown = ms(721);
    let unknown = holdover(&node, Span::ZERO, grown, Cause::UnknownEstimate);
    assert_eq!(reader.status(), unknown);
    assert_eq!(read(&reader), Some(measure(&node, Span::ZERO, grown)));
    let peer = clock.add();
    clock.push(peer, measure(&node, Span::ZERO, ms(1)));
    assert_eq!(reader.status(), synced(&node, Span::ZERO, ms(1)));
}

/// A held bound that drift grows to 36500 days is unknown, so the clock follows the
/// unknown estimate.
#[test]
fn follows_an_unknown_estimate_once_drift_makes_the_held_one_unknown() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [peer, os] = [clock.add(), clock.add()];
    let known = measure(&node, Span::ZERO, Span::from_nanos(UNKNOWN.nanos() - 1));
    let os_offset = Span::from_nanos(5 * Span::SECOND.nanos());
    clock.push(peer, known);
    clock.push(os, Measurement::unknown(node.clock().now(), os_offset));
    assert_eq!(reader.status(), Status::Synced(known));
    sim.run_for(Span::SECOND).expect("the run ends");
    clock.push(os, Measurement::unknown(node.clock().now(), os_offset));
    sim.run_for(Span::HOUR).expect("the run ends");
    // An hour at 500 ppm slews 1.8 s.
    let slewed = Measurement::unknown(node.clock().now(), ms(1800));
    assert_eq!(reader.status(), Status::Synced(slewed));
}

#[test]
fn holds_over_while_sources_split_then_follows_the_next_majority() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [a, b, c] = [clock.add(), clock.add(), clock.add()];
    let first = measure(&node, Span::ZERO, ms(1));
    let alone = Error::NoMajority {
        sources: 3,
        agreeing: 1,
        empty: 2,
    };
    clock.push(a, first);
    assert_eq!(reader.status(), Status::Unsynced(alone));
    for source in [b, c] {
        clock.push(source, first);
        assert_eq!(reader.status(), Status::Synced(first));
    }
    sim.run_for(Span::SECOND).expect("the run ends");
    // One second of drift adds 200 us to the error of each source.
    let grown = us(1_200);
    let ahead = measure(&node, Span::SECOND, ms(1));
    clock.push(b, ahead);
    assert_eq!(reader.status(), synced(&node, Span::ZERO, grown));
    let behind = measure(&node, ms(-1_000), ms(1));
    let split = Error::NoMajority {
        sources: 3,
        agreeing: 1,
        empty: 0,
    };
    clock.push(c, behind);
    assert_eq!(
        reader.status(),
        holdover(&node, Span::ZERO, grown, Cause::NoEstimate(split))
    );
    assert_eq!(read(&reader), Some(measure(&node, Span::ZERO, grown)));
    sim.run_for(Span::SECOND).expect("the run ends");
    let grown = us(1_400);
    assert_eq!(
        reader.status(),
        holdover(&node, Span::ZERO, grown, Cause::NoEstimate(split))
    );
    clock.push(b, measure(&node, Span::SECOND, ms(1)));
    assert_eq!(
        reader.status(),
        holdover(&node, Span::ZERO, grown, Cause::NoEstimate(split))
    );
    assert_eq!(read(&reader), Some(measure(&node, Span::ZERO, grown)));
    clock.push(c, measure(&node, Span::ZERO, ms(1)));
    assert_eq!(reader.status(), synced(&node, Span::ZERO, ms(1)));
}

#[test]
fn keeps_its_slew_in_holdover() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    clock.push(source, measure(&node, Span::ZERO, Span::ZERO));
    clock.push(source, measure(&node, us(400), Span::ZERO));
    let _: clock::source::Key = clock.add();
    let alone = Error::NoMajority {
        sources: 2,
        agreeing: 1,
        empty: 1,
    };
    assert_eq!(
        reader.status(),
        holdover(&node, Span::ZERO, us(400), Cause::NoEstimate(alone))
    );
    sim.run_for(ms(400)).expect("the run ends");
    assert_eq!(
        reader.status(),
        holdover(&node, us(200), us(280), Cause::NoEstimate(alone))
    );
    assert_eq!(read(&reader), Some(measure(&node, us(200), us(280))));
}

#[test]
fn steps_forward_to_an_estimate_far_ahead() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    clock.push(source, measure(&node, Span::ZERO, ms(1)));
    sim.run_for(Span::SECOND).expect("the run ends");
    // The earliest offset the estimate allows is 999 ms; its latest is 2 ms above.
    clock.push(source, measure(&node, Span::SECOND, ms(1)));
    assert_eq!(reader.status(), synced(&node, ms(999), ms(2)));
    assert_eq!(read(&reader), Some(measure(&node, ms(999), ms(2))));
}

#[test]
fn slews_toward_an_estimate_near_ahead() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    clock.push(source, measure(&node, Span::ZERO, Span::ZERO));
    clock.push(source, measure(&node, us(400), Span::ZERO));
    assert_eq!(read(&reader), Some(measure(&node, Span::ZERO, us(400))));
    // 400 ms at 500 ppm moves the offset 200 us, while drift widens the target by
    // 80 us.
    sim.run_for(ms(400)).expect("the run ends");
    assert_eq!(read(&reader), Some(measure(&node, us(200), us(280))));
}

#[test]
fn slews_at_500_ppm_however_often_sources_push() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    clock.push(source, measure(&node, Span::ZERO, Span::ZERO));
    for _ in 0..10_000 {
        clock.push(source, measure(&node, us(400), Span::ZERO));
        sim.run_for(us(1)).expect("the run ends");
    }
    // 10 ms at 500 ppm moves 5 us toward the estimate.
    let served = read(&reader).expect("synced").offset();
    assert_eq!(served, us(5), "mesh time stalled at {served}");
}

#[test]
fn a_remove_follows_the_sources_left_then_holds_over_with_none() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let [a, b] = [clock.add(), clock.add()];
    let both = measure(&node, Span::ZERO, ms(1));
    clock.push(a, both);
    clock.push(b, both);
    let split = Error::NoMajority {
        sources: 2,
        agreeing: 1,
        empty: 0,
    };
    clock.push(b, measure(&node, Span::SECOND, us(500)));
    assert_eq!(
        reader.status(),
        holdover(&node, Span::ZERO, ms(1), Cause::NoEstimate(split))
    );
    // The earliest offset b allows is 999.5 ms; its latest is 1 ms above.
    let stepped = us(999_500);
    clock.remove(a);
    assert_eq!(reader.status(), synced(&node, stepped, ms(1)));
    clock.remove(b);
    assert_eq!(
        reader.status(),
        holdover(&node, stepped, ms(1), Cause::NoEstimate(Error::NoSources))
    );
    assert_eq!(read(&reader), Some(measure(&node, stepped, ms(1))));
}

#[test]
fn a_removed_key_is_never_used_again() {
    let (_sim, node) = node();
    let (mut clock, _reader) = Clock::new(node.clock());
    let removed = clock.add();
    clock.remove(removed);
    assert_ne!(clock.add(), removed);
}

#[test]
fn a_push_or_remove_with_a_removed_key_panics() {
    let (_sim, node) = node();
    let (mut clock, _reader) = Clock::new(node.clock());
    let removed = clock.add();
    clock.remove(removed);
    let m = measure(&node, Span::ZERO, ms(1));
    let push = catch_unwind(AssertUnwindSafe(|| clock.push(removed, m)));
    let remove = catch_unwind(AssertUnwindSafe(|| clock.remove(removed)));
    for panic in [push, remove] {
        let message = panic.expect_err("panics");
        let message = message
            .downcast_ref::<String>()
            .expect("a formatted message");
        assert_eq!(message, &format!("{removed:?} was removed"));
    }
}

/// A true offset of mesh time from a node's monotonic clock that drifts by `ppm`.
#[derive(Clone, Copy)]
struct Truth {
    start: Monotonic,
    ppm: i64,
}

impl Truth {
    fn offset(self, now: Monotonic) -> i128 {
        let elapsed = i128::from(now.0 - self.start.0);
        i128::from(Span::HOUR.nanos()) + elapsed * i128::from(self.ppm) / 1_000_000
    }
}

const RUN: Span = Span::from_nanos(10 * Span::SECOND.nanos());

fn shard(core: usize) -> env::shards::Config {
    env::shards::Config {
        name: format!("shard-{core}"),
        core: Some(core),
    }
}

/// Every 50 to 150 ms, one source 10 s ahead and then three honest sources push a
/// measurement. The clock is unsynced until the third honest source first pushes.
async fn steer(
    monotonic: env::clock::Clock,
    mut clock: Clock,
    reader: Reader,
    truth: Truth,
) {
    let mut rng = env::rng::Rng::from_seed(truth.ppm.unsigned_abs());
    let sources = [clock.add(), clock.add(), clock.add(), clock.add()];
    let mut synced = false;
    let end = monotonic.now() + RUN;
    while monotonic.now() < end {
        for (i, &source) in sources.iter().enumerate() {
            let now = monotonic.now();
            let error = 100_000 + rng.below(50_000_000);
            let noise = i128::from(rng.below(2 * error + 1)) - i128::from(error);
            let lie = if i == 0 { 10 * Span::SECOND.nanos() } else { 0 };
            let offset = truth.offset(now) + noise + i128::from(lie);
            let offset = Span::from_nanos(i64::try_from(offset).expect("fits"));
            let error = Span::from_nanos(error.try_into().expect("fits"));
            let m = Measurement::new(now, offset, error).expect("at most 36500 days");
            clock.push(source, m);
            match reader.status() {
                Status::Synced(_) => synced = true,
                Status::Unsynced(_) if !synced && i < 3 => {}
                status => panic!("{status:?} from source {i}"),
            }
        }
        let pause = 50 + rng.below(100);
        monotonic.sleep(ms(pause.try_into().expect("fits"))).await;
    }
}

/// Every 1 to 1000 us, reads mesh time and the status and checks them against the
/// truth.
async fn check(monotonic: env::clock::Clock, reader: Reader, truth: Truth, core: u64) {
    let mut rng = env::rng::Rng::from_seed(core);
    let end = monotonic.now() + RUN;
    let mut last = None;
    while monotonic.now() < end {
        match reader.status() {
            Status::Synced(m) => {
                let time = i128::from(m.at().0) + truth.offset(m.at());
                let (low, high) = (m.interval().earliest, m.interval().latest);
                let (low, high) = (i128::from(low.nanos()), i128::from(high.nanos()));
                assert!((low..=high).contains(&time), "{m:?} misses {time}ns");
            }
            Status::Unsynced(_) => assert_eq!(last, None, "unsynced after mesh time"),
            status @ Status::Holdover(..) => panic!("{status:?}"),
        }
        let time = reader.now();
        if let Some(interval) = time.mesh {
            let now = time.monotonic;
            let time = i128::from(now.0) + truth.offset(now);
            let (earliest, latest) =
                (interval.earliest.nanos(), interval.latest.nanos());
            assert!(
                (i128::from(earliest)..=i128::from(latest)).contains(&time),
                "{interval:?} misses {time}ns at {now:?}"
            );
            let mid = earliest.midpoint(latest);
            assert!(last <= Some(mid), "mesh time went from {last:?} to {mid}");
            last = Some(mid);
        }
        let pause = 1 + rng.below(1_000);
        monotonic.sleep(us(pause.try_into().expect("fits"))).await;
    }
    assert!(last.is_some(), "the reader got mesh time");
}

#[test]
fn readers_on_every_shard_hold_the_truth_and_never_go_back() {
    for ppm in [-200, -73, 0, 120, 200] {
        let (mut sim, node) = node();
        let monotonic = node.clock();
        let truth = Truth {
            start: monotonic.now(),
            ppm,
        };
        let (clock, reader) = Clock::new(monotonic.clone());
        let mut shards = Vec::new();
        for core in 1..4 {
            let (monotonic, reader) = (monotonic.clone(), reader.clone());
            let start = node.shards().start(shard(core), move |_tasks| {
                check(monotonic, reader, truth, core as u64)
            });
            shards.push(start);
        }
        let monotonic = monotonic.clone();
        let start = node.shards().start(shard(0), move |_tasks| {
            steer(monotonic, clock, reader, truth)
        });
        shards.push(start);
        sim.run().expect("the run ends");
        for shard in shards {
            shard
                .expect("the shard starts")
                .join()
                .expect("the shard ends");
        }
    }
}
