//! The sources of a [`Clock`](crate::Clock): the key of each, and the adapters that
//! measure the node's monotonic clock against a time source.

use estimate::Measurement;
use estimate::exchange::{self, Exchange};
use types::time::{Interval, Monotonic, Span, Stamp};

use crate::DRIFT;

/// Identifies one source of a [`Clock`](crate::Clock). A removed key is never used
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub(crate) u64);

/// Measures the node's monotonic clock against the OS wall clock.
///
/// ```
/// #[expect(clippy::disallowed_methods, reason = "feeds the mesh clock")]
/// fn measure(
///     wall: env::wall::Wall,
///     monotonic: env::clock::Clock,
/// ) -> estimate::Measurement {
///     clock::source::Wall::new(wall, monotonic).measure()
/// }
/// ```
#[derive(Debug)]
pub struct Wall {
    wall: env::wall::Wall,
    monotonic: env::clock::Clock,
}

impl Wall {
    /// Reads `wall` against `monotonic`, the node's monotonic clock.
    #[must_use]
    pub fn new(wall: env::wall::Wall, monotonic: env::clock::Clock) -> Self {
        Self { wall, monotonic }
    }

    /// Reads the OS clock between two readings of the monotonic clock. The error is
    /// the OS bound plus half the time between the readings, grown by drift. With no
    /// OS bound, or an error over 36500 days, the measurement is unknown
    /// ([`Measurement::unknown`]).
    ///
    /// # Panics
    ///
    /// When the OS bound is negative, or the monotonic clock goes back.
    #[must_use]
    pub fn measure(&self) -> Measurement {
        let sent = self.monotonic.now();
        #[expect(clippy::disallowed_methods, reason = "clock reads the OS clock")]
        let reading = self.wall.now();
        let returned = self.monotonic.now();
        let bound = reading.error.inspect(|&bound| {
            assert!(
                bound >= Span::ZERO,
                "invariant: the OS error bound {bound} is negative"
            );
        });
        let instant = Interval {
            earliest: reading.time,
            latest: reading.time,
        };
        let exchange = Exchange {
            sent,
            received: instant,
            answered: instant,
            returned,
        };
        let read = match exchange.measure(DRIFT) {
            Ok(read) => read,
            Err(exchange::Error::Bound { .. }) => {
                return Measurement::unknown(
                    returned,
                    offset(reading.time, sent, returned),
                );
            }
            // The OS reading is one instant, so only a clock that goes back crosses.
            Err(exchange::Error::Crossed) => {
                panic!(
                    "invariant: the monotonic clock went from {sent:?} to {returned:?}"
                )
            }
        };
        let unknown = Measurement::unknown(read.at(), read.offset());
        let Some(bound) = bound else {
            return unknown;
        };
        let error =
            Span::from_nanos(read.error().nanos().saturating_add(bound.nanos()));
        Measurement::new(read.at(), read.offset(), error).unwrap_or(unknown)
    }
}

/// `time` minus the midpoint of `sent` and `returned`, or the nearest span when it is
/// past that range.
fn offset(time: Stamp, sent: Monotonic, returned: Monotonic) -> Span {
    let mid = sent.0.midpoint(returned.0);
    let nanos = (i128::from(time.nanos()) - i128::from(mid))
        .clamp(i64::MIN.into(), i64::MAX.into());
    Span::from_nanos(i64::try_from(nanos).expect("invariant: clamped to a span"))
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

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

    /// The measurement of a simulated node whose OS gives `wall` and `wall_error`.
    fn measure_at(wall: Stamp, wall_error: Option<Span>) -> Measurement {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(node::Config {
            wall,
            wall_error,
            ..node::Config::default()
        });
        measure(&node)
    }

    #[expect(clippy::disallowed_methods, reason = "the tests measure")]
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

    fn days(n: i64) -> Span {
        Span::from_nanos(n * Span::DAY.nanos())
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
    fn keeps_the_os_bound_near_each_end_of_the_stamps() {
        for wall in [i64::MAX - Span::DAY.nanos(), i64::MIN + Span::DAY.nanos()] {
            let m = measure_at(Stamp::from_nanos(wall), Some(Span::DAY));
            assert_eq!(m.error(), Span::DAY);
            let m = measure_at(Stamp::from_nanos(wall), Some(days(40_000)));
            assert_eq!(m.error(), UNKNOWN);
        }
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

    /// A monotonic clock that gives its readings in order, one per call.
    struct Readings([u64; 2], AtomicUsize);

    impl env::clock::Driver for Readings {
        fn now(&self) -> Monotonic {
            Monotonic(self.0[self.1.fetch_add(1, Ordering::Relaxed)])
        }

        fn epoch(&self) -> Instant {
            unreachable!("measure reads only now")
        }

        fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
            unreachable!("measure reads only now")
        }
    }

    /// An OS clock that always gives one reading.
    struct Os(env::wall::Reading);

    impl env::wall::Driver for Os {
        fn now(&self) -> env::wall::Reading {
            self.0
        }
    }

    /// The measurement of an OS clock that gives `time` and `error` while the
    /// monotonic clock reads `sent`, then `returned`.
    #[expect(clippy::disallowed_methods, reason = "the tests measure")]
    fn measure_between(
        sent: u64,
        returned: u64,
        time: i64,
        error: Option<Span>,
    ) -> Measurement {
        let monotonic =
            env::clock::Clock::new(Readings([sent, returned], AtomicUsize::new(0)));
        let time = Stamp::from_nanos(time);
        let wall = env::wall::Wall::new(Os(env::wall::Reading { time, error }));
        Wall::new(wall, monotonic).measure()
    }

    #[test]
    fn adds_the_os_bound_to_half_the_read() {
        let (sent, returned, time) = (1_000_000_000, 1_001_000_000, 10_000_000_000);
        let at = Monotonic(returned);
        let offset = Span::from_nanos(8_999_500_100);
        let error = Span::from_nanos(500_100 + 1_000_000);
        let m = measure_between(sent, returned, time, Some(Span::MILLISECOND));
        assert_eq!(Some(m), Measurement::new(at, offset, error));
        let m = measure_between(sent, returned, time, None);
        assert_eq!(m, Measurement::unknown(at, offset));
    }

    #[test]
    fn is_unknown_at_the_midpoint_when_the_reads_are_too_far_apart() {
        let returned = 7_000_000_000_000_000_000;
        let m = measure_between(0, returned, 10_000_000_000, Some(Span::ZERO));
        let offset = Span::from_nanos(-3_499_999_990_000_000_000);
        assert_eq!(m, Measurement::unknown(Monotonic(returned), offset));
    }

    #[test]
    fn is_unknown_when_the_offset_is_far_past_a_span() {
        let m = measure_between(u64::MAX, u64::MAX, i64::MIN, Some(Span::ZERO));
        let offset = Span::from_nanos(i64::MIN);
        assert_eq!(m, Measurement::unknown(Monotonic(u64::MAX), offset));
    }

    #[test]
    #[should_panic(
        expected = "the monotonic clock went from Monotonic(2) to Monotonic(1)"
    )]
    fn panics_when_the_monotonic_clock_goes_back() {
        let _ = measure_between(2, 1, 0, None);
    }

    proptest! {
        #[test]
        fn takes_the_os_reading_with_its_bound(
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
            // Only the low end of a span can cut the offset, and the error covers the
            // cut.
            let at = Monotonic(monotonic);
            let exact = i128::from(wall) - i128::from(monotonic);
            let offset = i64::try_from(exact).unwrap_or(i64::MIN);
            let cut = i128::from(offset) - exact;
            let offset = Span::from_nanos(offset);
            let expected = error
                .and_then(|error| i64::try_from(i128::from(error) + cut).ok())
                .and_then(|error| Measurement::new(at, offset, Span::from_nanos(error)))
                .unwrap_or(Measurement::unknown(at, offset));
            prop_assert_eq!(measure(&node), expected);
        }
    }
}
