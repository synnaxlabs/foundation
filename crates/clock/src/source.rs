//! Adapters that measure the node's monotonic clock against a time source.

use estimate::exchange::{self, Exchange};
use estimate::{Drift, Measurement};
use types::time::{Interval, Monotonic, Span, Stamp};

/// Measures the node's monotonic clock against the OS wall clock.
///
/// ```
/// fn measure(wall: env::wall::Wall, clock: env::clock::Clock) -> estimate::Measurement {
///     clock::source::Wall::new(wall, clock).measure()
/// }
/// ```
#[derive(Debug)]
pub struct Wall {
    wall: env::wall::Wall,
    clock: env::clock::Clock,
}

impl Wall {
    /// Reads `wall` against `clock`, the node's monotonic clock.
    #[must_use]
    pub fn new(wall: env::wall::Wall, clock: env::clock::Clock) -> Self {
        Self { wall, clock }
    }

    /// Reads the OS clock between two readings of the monotonic clock. The error is
    /// the OS bound plus half the time between the readings, grown by drift. With no
    /// OS bound, or a bound over 36500 days, the measurement is unknown
    /// ([`Measurement::unknown`]).
    ///
    /// # Panics
    ///
    /// When the OS bound is negative.
    #[must_use]
    pub fn measure(&self) -> Measurement {
        let sent = self.clock.now();
        #[expect(clippy::disallowed_methods, reason = "clock reads the OS clock")]
        let reading = self.wall.now();
        let returned = self.clock.now();
        let unknown = || Measurement::unknown(returned, offset(reading.time, returned));
        let Some(error) = reading.error else {
            return unknown();
        };
        assert!(
            error >= Span::ZERO,
            "invariant: the OS error bound {error} is negative"
        );
        let edge =
            |stamp: Option<Stamp>, stop| stamp.unwrap_or(Stamp::from_nanos(stop));
        let around = Interval {
            earliest: edge(reading.time.checked_sub(error), i64::MIN),
            latest: edge(reading.time.checked_add(error), i64::MAX),
        };
        let exchange = Exchange {
            sent,
            received: around,
            answered: around,
            returned,
        };
        match exchange.measure(Drift::UNDISCIPLINED) {
            Ok(m) => m,
            Err(exchange::Error::Bound { .. }) => unknown(),
            Err(exchange::Error::Crossed) => {
                panic!(
                    "invariant: the monotonic clock went from {sent:?} to {returned:?}"
                )
            }
        }
    }
}

/// `time` minus `at`, saturated to a span.
fn offset(time: Stamp, at: Monotonic) -> Span {
    let nanos = i128::from(time.nanos()) - i128::from(at.0);
    let stop = if nanos < 0 { i64::MIN } else { i64::MAX };
    Span::from_nanos(i64::try_from(nanos).unwrap_or(stop))
}

#[cfg(test)]
mod tests {
    use estimate::Measurement;
    use proptest::prelude::*;
    use sim::node::{self, Node};
    use types::time::{Monotonic, Span, Stamp};

    use super::Wall;

    /// 36500 days, the largest error a measurement has.
    const UNKNOWN: Span = Span::from_nanos(36_500 * Span::DAY.nanos());

    /// A simulated node whose OS gives `wall_error`, with the default clocks.
    fn node(wall_error: Option<Span>) -> (sim::Sim, Node) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let config = node::Config {
            wall_error,
            ..node::Config::default()
        };
        let node = sim.node(config);
        (sim, node)
    }

    fn measure(node: &Node) -> Measurement {
        Wall::new(node.wall(), node.clock()).measure()
    }

    /// The default node's wall time minus its monotonic reading.
    fn offset() -> Span {
        let config = node::Config::default();
        let monotonic = i64::try_from(config.monotonic.0).expect("one hour fits");
        Span::from_nanos(config.wall.nanos() - monotonic)
    }

    fn at() -> Monotonic {
        node::Config::default().monotonic
    }

    #[test]
    fn takes_the_os_bound() {
        let ms = Span::from_nanos(10 * Span::MILLISECOND.nanos());
        let (_sim, node) = node(Some(ms));
        assert_eq!(Some(measure(&node)), Measurement::new(at(), offset(), ms));
    }

    #[test]
    fn is_unknown_with_no_os_bound() {
        let (_sim, node) = node(None);
        assert_eq!(measure(&node), Measurement::unknown(at(), offset()));
    }

    #[test]
    fn is_unknown_with_an_os_bound_over_36500_days() {
        let (_sim, node) = node(Some(Span::from_nanos(UNKNOWN.nanos() + 1)));
        assert_eq!(measure(&node), Measurement::unknown(at(), offset()));
    }

    #[test]
    fn is_unknown_with_an_os_bound_past_every_stamp() {
        let (_sim, node) = node(Some(Span::from_nanos(i64::MAX)));
        assert_eq!(measure(&node), Measurement::unknown(at(), offset()));
    }

    #[test]
    fn follows_a_step_of_the_os_clock() {
        let (_sim, node) = node(None);
        node.step_wall(Span::HOUR);
        let offset = Span::from_nanos(offset().nanos() + Span::HOUR.nanos());
        assert_eq!(measure(&node), Measurement::unknown(at(), offset));
    }

    #[test]
    #[should_panic(expected = "the OS error bound -1ns is negative")]
    fn panics_on_a_negative_os_bound() {
        let (_sim, node) = node(Some(Span::from_nanos(-1)));
        let _ = measure(&node);
    }

    proptest! {
        #[test]
        fn holds_the_os_time_within_a_known_bound(
            monotonic in any::<u64>(),
            wall in any::<i64>(),
            error in proptest::option::of(0..=i64::MAX),
        ) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(node::Config {
                monotonic: Monotonic(monotonic),
                wall: Stamp::from_nanos(wall),
                wall_error: error.map(Span::from_nanos),
                ..node::Config::default()
            });
            let m = measure(&node);
            prop_assert_eq!(m.at(), Monotonic(monotonic));
            if m.error() < UNKNOWN {
                let (interval, time) = (m.interval(), Stamp::from_nanos(wall));
                prop_assert!(interval.earliest <= time && time <= interval.latest);
            }
            let fits = |nanos: i128| i64::try_from(nanos).is_ok();
            let (wall, offset) = (i128::from(wall), i128::from(wall) - i128::from(monotonic));
            if let Some(error) = error.filter(|&e| e <= UNKNOWN.nanos())
                && fits(offset)
                && fits(wall - i128::from(error))
                && fits(wall + i128::from(error))
            {
                prop_assert_eq!(m.error(), Span::from_nanos(error));
            }
        }
    }
}
