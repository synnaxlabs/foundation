//! The clock exchange with a peer: the answer this node gives, and the measurement
//! that an answer gives the node that asks.

use estimate::Measurement;
use estimate::exchange::{Exchange, Reading};
use types::time::Monotonic;
use wire::clock::{Answer, Request, Time};

use crate::{DRIFT, Reader, Status, source};

/// The answer to `request` from this node's time now: mesh time from `reader`, or
/// the OS clock's time from `os` while the clock has no mesh time. A bound of 36500
/// days gives [`Time::Unknown`] with the best guess.
///
/// # Panics
///
/// As [`source::Wall::measure`], while the clock has no mesh time.
pub(crate) fn answer(reader: &Reader, os: &source::Wall, request: Request) -> Answer {
    let m = match reader.status() {
        Status::Synced(m) | Status::Holdover(m, _) => m,
        #[expect(clippy::disallowed_methods, reason = "answers with the OS clock")]
        Status::Unsynced(_) => os.measure(),
    };
    // One read for both: two reads can straddle a sync and pair a known interval with
    // an unknown one.
    let time = if m.known() {
        Time::Known {
            received: m.interval(),
            answered: m.interval(),
        }
    } else {
        Time::Unknown { answered: m.time() }
    };
    Answer {
        sent: request.sent,
        time,
    }
}

/// The measurement from `answer`, which arrived at `returned`. An error of 36500 days
/// or more, or a [`Time::Unknown`], gives an unknown measurement. `None` when the
/// answer allows no offset: an interval is inverted, the peer's time goes back or moves
/// on more than the round trip allows, or `sent` is after `returned`.
pub(crate) fn measure(answer: Answer, returned: Monotonic) -> Option<Measurement> {
    let reading = match answer.time {
        Time::Known { received, answered } => Reading::Known { received, answered },
        Time::Unknown { answered } => Reading::Unknown(answered),
    };
    let exchange = Exchange {
        sent: answer.sent,
        reading,
        returned,
    };
    exchange.measure(DRIFT)
}

#[cfg(test)]
mod tests {
    use estimate::Measurement;
    use proptest::prelude::*;
    use sim::node::{self, Node};
    use types::time::{Interval, Monotonic, Span, Stamp};
    use wire::clock::{Answer, Request, Time};

    use super::{answer, measure};
    use crate::{Clock, Reader, source};

    /// The error of an unknown measurement.
    const UNKNOWN: Span = Measurement::unknown(Monotonic(0), Span::ZERO).error();

    /// 1 January 2026.
    const TODAY: i64 = 1_767_225_600 * 1_000_000_000;

    fn node(config: node::Config) -> (sim::Sim, Node) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(config);
        (sim, node)
    }

    /// A node whose OS gives `wall_error`, with a clock that has no sources.
    fn unsynced(wall_error: Option<Span>) -> (sim::Sim, Node, Clock, Reader) {
        let (sim, node) = node(node::Config {
            wall_error,
            ..node::Config::default()
        });
        let (clock, reader) = Clock::new(node.clock());
        (sim, node, clock, reader)
    }

    /// A node whose clock has one source that pushed `m`, at the node's monotonic
    /// reading `at`.
    fn synced(at: Monotonic, m: Measurement) -> (sim::Sim, Node, Clock, Reader) {
        let (sim, node) = node(node::Config {
            monotonic: at,
            ..node::Config::default()
        });
        let (mut clock, reader) = Clock::new(node.clock());
        let source = clock.add();
        clock.push(source, m);
        (sim, node, clock, reader)
    }

    /// The node's answer to its own request now.
    fn answer_now(node: &Node, reader: &Reader) -> Answer {
        let os = source::Wall::new(node.wall(), node.clock());
        let sent = node.clock().now();
        answer(reader, &os, Request { sent })
    }

    /// What the node measures when it asks itself at one instant.
    fn ask(node: &Node, reader: &Reader) -> Option<Measurement> {
        measure(answer_now(node, reader), node.clock().now())
    }

    fn at() -> Monotonic {
        node::Config::default().monotonic
    }

    fn known(offset: i64, error: Span) -> Measurement {
        Measurement::new(at(), Span::from_nanos(offset), error).expect("valid")
    }

    fn stamps(earliest: i64, latest: i64) -> Interval {
        Interval {
            earliest: Stamp::from_nanos(earliest),
            latest: Stamp::from_nanos(latest),
        }
    }

    fn known_answer(sent: u64, received: Interval, answered: Interval) -> Answer {
        Answer {
            sent: Monotonic(sent),
            time: Time::Known { received, answered },
        }
    }

    fn unknown_answer(sent: u64, answered: i64) -> Answer {
        Answer {
            sent: Monotonic(sent),
            time: Time::Unknown {
                answered: Stamp::from_nanos(answered),
            },
        }
    }

    mod answer {
        use super::*;

        #[test]
        fn sends_one_interval_of_mesh_time_for_both() {
            let m = known(TODAY, Span::MILLISECOND);
            let (_sim, node, _clock, reader) = synced(at(), m);
            let time = Time::Known {
                received: m.interval(),
                answered: m.interval(),
            };
            let sent = at();
            assert_eq!(answer_now(&node, &reader), Answer { sent, time });
        }

        #[test]
        fn sends_mesh_time_in_holdover() {
            let m = known(TODAY, Span::MILLISECOND);
            let (_sim, node, mut clock, reader) = synced(at(), m);
            let _: source::Key = clock.add();
            let time = Time::Known {
                received: m.interval(),
                answered: m.interval(),
            };
            assert_eq!(answer_now(&node, &reader).time, time);
        }

        #[test]
        fn sends_the_guess_of_an_unknown_estimate() {
            let m = Measurement::unknown(at(), Span::from_nanos(TODAY));
            let (_sim, node, _clock, reader) = synced(at(), m);
            let answered = m.time();
            assert_eq!(answer_now(&node, &reader).time, Time::Unknown { answered });
        }

        #[test]
        fn sends_the_os_clock_while_unsynced() {
            let ms = Span::from_nanos(10 * Span::MILLISECOND.nanos());
            let (_sim, node, _clock, reader) = unsynced(Some(ms));
            let os = node::Config::default().wall;
            let interval = stamps(os.nanos() - ms.nanos(), os.nanos() + ms.nanos());
            let time = Time::Known {
                received: interval,
                answered: interval,
            };
            assert_eq!(answer_now(&node, &reader).time, time);
            let (_sim, node, _clock, reader) = unsynced(None);
            let time = Time::Unknown { answered: os };
            assert_eq!(answer_now(&node, &reader).time, time);
        }
    }

    mod measure {
        use super::*;

        #[test]
        fn takes_the_offset_and_error_of_a_round_trip() {
            let peer = stamps(TODAY - 10_000, TODAY + 10_000);
            let m = measure(known_answer(0, peer, peer), Monotonic(1_000_000));
            // Half the round trip and the drift over it, plus the peer's error.
            let offset = Span::from_nanos(TODAY - 1_000_000 + 500_100);
            let error = Span::from_nanos(500_100 + 10_000);
            assert_eq!(m, Measurement::new(Monotonic(1_000_000), offset, error));
        }

        #[test]
        fn makes_an_unknown_answer_unknown_at_its_guess() {
            let m = measure(unknown_answer(0, TODAY), Monotonic(1_000_000));
            let offset = Span::from_nanos(TODAY - 1_000_000 + 500_100);
            let unknown = Measurement::unknown(Monotonic(1_000_000), offset);
            assert_eq!(m, Some(unknown));
        }

        #[test]
        fn is_unknown_for_a_known_answer_over_36500_days() {
            let wide = stamps(TODAY - UNKNOWN.nanos(), TODAY + UNKNOWN.nanos());
            let m = measure(known_answer(0, wide, wide), Monotonic(1_000_000));
            let offset = Span::from_nanos(TODAY - 1_000_000 + 500_100);
            let unknown = Measurement::unknown(Monotonic(1_000_000), offset);
            assert_eq!(m, Some(unknown));
        }

        #[test]
        fn allows_no_offset_from_an_answer_that_cannot_be() {
            let peer = stamps(TODAY, TODAY);
            let returned = Monotonic(1_000);
            let inverted = stamps(TODAY + 1, TODAY);
            assert_eq!(measure(known_answer(0, inverted, peer), returned), None);
            assert_eq!(measure(known_answer(0, peer, inverted), returned), None);
            let back = stamps(TODAY - 1, TODAY - 1);
            assert_eq!(measure(known_answer(0, peer, back), returned), None);
            let on = stamps(TODAY + 2_000, TODAY + 2_000);
            assert_eq!(measure(known_answer(0, peer, on), returned), None);
            assert_eq!(measure(known_answer(1_001, peer, peer), returned), None);
            assert_eq!(measure(unknown_answer(1_001, TODAY), returned), None);
        }
    }

    mod ask {
        use super::*;

        #[test]
        fn measures_its_mesh_time() {
            let m = known(TODAY, Span::MILLISECOND);
            let (_sim, node, mut clock, reader) = synced(at(), m);
            assert_eq!(ask(&node, &reader), Some(m));
            let _: source::Key = clock.add();
            assert_eq!(ask(&node, &reader), Some(m), "in holdover");
        }

        #[test]
        fn measures_its_os_clock_while_unsynced() {
            for error in [Some(Span::MILLISECOND), None] {
                let (_sim, node, _clock, reader) = unsynced(error);
                #[expect(clippy::disallowed_methods, reason = "the test reads the OS")]
                let os = source::Wall::new(node.wall(), node.clock()).measure();
                assert_eq!(ask(&node, &reader), Some(os), "{error:?}");
            }
        }

        /// After 2162 the late edge of an unknown interval stops at the last stamp,
        /// so the interval is narrower than an unknown bound.
        #[test]
        fn is_unknown_after_2162_with_an_unknown_estimate() {
            let late = i64::MAX - Span::DAY.nanos();
            let m = Measurement::unknown(at(), Span::from_nanos(late));
            let (_sim, node, _clock, reader) = synced(at(), m);
            assert_eq!(ask(&node, &reader), Some(m));
        }

        /// Before 1777 the early edge of an unknown interval stops at the first stamp.
        #[test]
        fn is_unknown_before_1777_with_an_unknown_estimate() {
            let early = i64::MIN + Span::DAY.nanos();
            let m = Measurement::unknown(at(), Span::from_nanos(early));
            let (_sim, node, _clock, reader) = synced(at(), m);
            assert_eq!(ask(&node, &reader), Some(m));
        }

        fn fits(ns: i128) -> bool {
            i64::try_from(ns).is_ok()
        }

        proptest! {
            #[test]
            fn measures_a_known_status_while_its_interval_fits_stamps(
                monotonic in any::<u64>(),
                offset in any::<i64>(),
                error in 0..UNKNOWN.nanos(),
            ) {
                let (at, offset) = (Monotonic(monotonic), Span::from_nanos(offset));
                let m = Measurement::new(at, offset, Span::from_nanos(error));
                let m = m.expect("valid");
                let (_sim, node, _clock, reader) = synced(at, m);
                let time = i128::from(monotonic) + i128::from(offset.nanos());
                let asked = ask(&node, &reader);
                prop_assert!(asked.is_some());
                let error = i128::from(error);
                if fits(time - error) && fits(time + error) {
                    prop_assert_eq!(asked, Some(m));
                }
            }

            #[test]
            fn measures_an_unknown_status_while_its_guess_fits_a_stamp(
                monotonic in any::<u64>(),
                offset in any::<i64>(),
            ) {
                let (at, offset) = (Monotonic(monotonic), Span::from_nanos(offset));
                let m = Measurement::unknown(at, offset);
                let (_sim, node, _clock, reader) = synced(at, m);
                let time = i128::from(monotonic) + i128::from(offset.nanos());
                let asked = ask(&node, &reader);
                prop_assert!(asked.is_some());
                if fits(time) {
                    prop_assert_eq!(asked, Some(m));
                }
            }
        }
    }
}
