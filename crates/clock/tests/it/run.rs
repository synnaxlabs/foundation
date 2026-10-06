use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use clock::{Clock, Reader, Status};
use estimate::combine::Error;
use sim::Sim;
use sim::node::{self, Node};
use types::time::{Interval, Span, Stamp};

use crate::common::{UNKNOWN, ms, node};

/// Runs a clock on one shard of `host`, fed by `wall`.
fn run(host: &Node, wall: env::wall::Wall) -> Reader {
    let (clock, reader) = Clock::new(host.clock());
    start(host, clock, wall);
    reader
}

/// Runs `clock` on one shard of `host`, fed by `wall`.
fn start(host: &Node, clock: Clock, wall: env::wall::Wall) {
    let shard = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let handle = host
        .shards()
        .start(shard, |_tasks| async { clock.run(wall).await });
    drop(handle.expect("the shard starts"));
}

/// Mesh time and the OS clock now.
fn read(host: &Node, reader: &Reader) -> (Interval, Stamp) {
    let mesh = reader.now().expect("mesh time");
    #[expect(clippy::disallowed_methods, reason = "the test reads the truth")]
    let wall = host.wall().now().time;
    (mesh, wall)
}

fn midpoint(interval: Interval) -> Stamp {
    let (earliest, latest) = (interval.earliest.nanos(), interval.latest.nanos());
    Stamp::from_nanos(earliest.midpoint(latest))
}

fn holds(interval: Interval, wall: Stamp) -> bool {
    interval.earliest <= wall && wall <= interval.latest
}

#[test]
fn serves_the_os_clock_at_once() {
    let (mut sim, host) = node();
    let reader = run(&host, host.wall());
    assert_eq!(reader.now(), None);
    assert_eq!(reader.status(), Status::Unsynced(Error::NoSources));
    sim.run_for(Span::ZERO).expect("runs");
    let (mesh, wall) = read(&host, &reader);
    let expected = Interval {
        earliest: wall - ms(10),
        latest: wall + ms(10),
    };
    assert_eq!(mesh, expected);
    let Status::Synced(m) = reader.status() else {
        panic!("{:?}", reader.status());
    };
    assert_eq!(m.interval(), expected);
}

#[test]
fn serves_unknown_time_at_once_with_no_os_bound() {
    let mut sim = Sim::new(sim::Config::default());
    let host = sim.node(node::Config {
        wall_error: None,
        ..node::Config::default()
    });
    let reader = run(&host, host.wall());
    sim.run_for(Span::ZERO).expect("runs");
    let (mesh, wall) = read(&host, &reader);
    let expected = Interval {
        earliest: wall - UNKNOWN,
        latest: wall + UNKNOWN,
    };
    assert_eq!(mesh, expected);
}

#[test]
fn keeps_a_narrow_measurement_when_the_os_bound_widens() {
    let (mut sim, host) = node();
    let reader = run(&host, host.wall());
    sim.run_for(Span::ZERO).expect("runs");
    host.set_wall_error(Some(Span::SECOND));
    sim.run_for(Span::SECOND).expect("runs");
    let (mesh, wall) = read(&host, &reader);
    // The first measurement, grown by drift for a second.
    let error = Span::from_nanos(10_200_000);
    let expected = Interval {
        earliest: wall - error,
        latest: wall + error,
    };
    assert_eq!(mesh, expected);
}

#[test]
fn a_source_added_before_run_panics() {
    let (mut sim, host) = node();
    let (mut clock, _reader) = Clock::new(host.clock());
    let _ = clock.add();
    start(&host, clock, host.wall());
    assert_eq!(
        sim.run_for(Span::ZERO),
        Err(sim::Error::Panicked {
            thread: "shard-0".into(),
            message: "a source was added before run".into(),
            seed: 0,
        })
    );
}

#[test]
fn steps_with_a_step_forward_of_the_os_clock_within_a_second() {
    let (mut sim, host) = node();
    let reader = run(&host, host.wall());
    sim.run_for(Span::ZERO).expect("runs");
    host.step_wall(Span::SECOND);
    sim.run_for(Span::SECOND).expect("runs");
    let (mesh, wall) = read(&host, &reader);
    assert!(holds(mesh, wall), "{mesh:?} misses {wall}");
    assert!(
        midpoint(mesh) >= wall - ms(10),
        "slewed: {mesh:?} at {wall}"
    );
}

#[test]
fn slews_after_a_step_back_of_the_os_clock_and_never_goes_back() {
    let (mut sim, host) = node();
    let reader = run(&host, host.wall());
    sim.run_for(Span::ZERO).expect("runs");
    let mut last = midpoint(read(&host, &reader).0);
    host.step_wall(Span::from_nanos(-Span::SECOND.nanos()));
    sim.run_for(Span::SECOND).expect("runs");
    for _ in 0..100 {
        sim.run_for(ms(100)).expect("runs");
        let (mesh, wall) = read(&host, &reader);
        assert!(holds(mesh, wall), "{mesh:?} misses {wall}");
        let now = midpoint(mesh);
        assert!(now >= last, "went back from {last} to {now}");
        assert!(now - wall > ms(990), "stepped back: {mesh:?} at {wall}");
        last = now;
    }
    let (mesh, wall) = read(&host, &reader);
    assert!(midpoint(mesh) - wall <= ms(995), "did not slew: {mesh:?}");
}

#[test]
fn measures_again_after_a_suspend() {
    let (mut sim, host) = node();
    let reader = run(&host, host.wall());
    sim.run_for(Span::ZERO).expect("runs");
    host.pause(Span::HOUR);
    sim.run_for(Span::HOUR).expect("runs");
    sim.run_for(ms(1)).expect("runs");
    let (mesh, wall) = read(&host, &reader);
    assert!(holds(mesh, wall), "{mesh:?} misses {wall}");
    // With no measurement since the suspend, drift makes it 1.46 s wide.
    let width = mesh.latest - mesh.earliest;
    assert!(width < ms(22), "{width} wide after a suspend");
}

/// The host's OS clock, counting its reads.
struct Counted(env::wall::Wall, Arc<AtomicUsize>);

impl env::wall::Driver for Counted {
    fn now(&self) -> env::wall::Reading {
        self.1.fetch_add(1, Ordering::Relaxed);
        #[expect(clippy::disallowed_methods, reason = "the test counts reads")]
        self.0.now()
    }
}

#[test]
fn reads_the_os_clock_once_a_second_and_once_after_a_suspend() {
    let (mut sim, host) = node();
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Counted(host.wall(), Arc::clone(&reads));
    let _reader = run(&host, env::wall::Wall::new(counted));
    sim.run_for(Span::ZERO).expect("runs");
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    sim.run_for(Span::from_nanos(10 * Span::SECOND.nanos()))
        .expect("runs");
    assert_eq!(reads.load(Ordering::Relaxed), 11);
    host.pause(Span::HOUR);
    sim.run_for(Span::HOUR).expect("runs");
    sim.run_for(ms(1)).expect("runs");
    assert_eq!(reads.load(Ordering::Relaxed), 12);
}
