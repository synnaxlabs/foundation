//! The OS clock as a source of mesh time.

use clock::Clock;
use clock::source::Wall;
use types::time::Span;

/// How often the node measures the OS clock. A step of the OS clock reaches mesh time
/// within one period.
const PERIOD: Span = Span::SECOND;

/// Feeds `clock` a measurement of `wall` at once, then once a period. `monotonic` is
/// the clock that `wall` reads against. It never ends; its shard drops it.
pub(crate) async fn run(
    mut clock: Clock,
    wall: Wall,
    monotonic: env::clock::Clock,
) -> ! {
    let source = clock.add();
    let mut sleep = monotonic.sleep(Span::ZERO);
    loop {
        #[expect(clippy::disallowed_methods, reason = "node feeds the mesh clock")]
        clock.push(source, wall.measure());
        // From now, not from the last deadline: one measurement after a suspend, not
        // one for each period it missed.
        sleep.reset(monotonic.now() + PERIOD);
        (&mut sleep).await;
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use clock::source::Wall;
    use clock::{Clock, Reader};
    use sim::node::Node;
    use types::time::{Interval, Span, Stamp};

    use super::run;

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * Span::MILLISECOND.nanos())
    }

    /// Runs the loop on one shard of a host whose OS gives a bound of 10 ms.
    fn start() -> (sim::Sim, Node, Reader) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = sim.node(sim::node::Config::default());
        let (clock, reader) = Clock::new(host.clock());
        let wall = Wall::new(host.wall(), host.clock());
        let monotonic = host.clock();
        let shard = env::shards::Config {
            name: "shard-0".into(),
            core: Some(0),
        };
        let handle = host
            .shards()
            .start(shard, |_tasks| async { run(clock, wall, monotonic).await });
        drop(handle.expect("the shard starts"));
        (sim, host, reader)
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
        let (mut sim, host, reader) = start();
        assert_eq!(reader.now(), None);
        sim.run_for(Span::ZERO).expect("runs");
        let (mesh, wall) = read(&host, &reader);
        let expected = Interval {
            earliest: wall - ms(10),
            latest: wall + ms(10),
        };
        assert_eq!(mesh, expected);
    }

    #[test]
    fn steps_with_a_step_forward_of_the_os_clock_within_a_second() {
        let (mut sim, host, reader) = start();
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
        let (mut sim, host, reader) = start();
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
    }

    #[test]
    fn measures_again_after_a_suspend() {
        let (mut sim, host, reader) = start();
        sim.run_for(Span::ZERO).expect("runs");
        host.pause(Span::HOUR);
        sim.run_for(Span::HOUR).expect("runs");
        sim.run_for(ms(1)).expect("runs");
        let (mesh, wall) = read(&host, &reader);
        assert!(holds(mesh, wall), "{mesh:?} misses {wall}");
        // With no measurement since the suspend, drift alone makes it 3.6 s wide.
        let width = mesh.latest - mesh.earliest;
        assert!(width < ms(22), "{width} wide after a suspend");
    }
}
