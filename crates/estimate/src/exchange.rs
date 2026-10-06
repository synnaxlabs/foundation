//! Measurements from round trips to another clock.

use types::time::{Interval, Monotonic};

use crate::{Drift, Measurement};

/// One reading of another clock's mesh time, taken between two readings of the local
/// monotonic clock.
///
/// ```
/// use estimate::Drift;
/// use estimate::exchange::Exchange;
/// use types::time::{Interval, Monotonic, Span, Stamp};
///
/// let peer = |ns: i64| Interval {
///     earliest: Stamp::from_nanos(ns - 10),
///     latest: Stamp::from_nanos(ns + 10),
/// };
/// let exchange = Exchange {
///     sent: Monotonic(1_000),
///     received: peer(6_100),
///     answered: peer(6_150),
///     returned: Monotonic(1_250),
/// };
/// let m = exchange.measure(Drift::from_ppb(0).expect("at most 10%"));
/// let ns = Span::from_nanos;
/// assert_eq!(m.map(|m| (m.offset(), m.error())), Some((ns(5_000), ns(110))));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exchange {
    /// The local monotonic reading when the request left.
    pub sent: Monotonic,
    /// The other clock's mesh time when the request arrived.
    pub received: Interval,
    /// The other clock's mesh time when it answered.
    pub answered: Interval,
    /// The local monotonic reading when the answer arrived.
    pub returned: Monotonic,
}

impl Exchange {
    /// The offset of the local monotonic clock at `returned`, for a clock that drifts
    /// from mesh time by at most `drift`. When both intervals hold the other clock's
    /// true mesh time, it holds the true offset whatever the delay in each direction.
    /// An error over 36500 days gives an unknown measurement
    /// ([`Measurement::unknown`]), centered between the edges or at the nearest span.
    /// `None` when the exchange allows no offset: an interval is inverted, the other
    /// clock goes back, the local clock drifts more than the drift bound, or `sent` is
    /// after `returned`.
    #[must_use]
    pub fn measure(self, drift: Drift) -> Option<Measurement> {
        let (received, answered) = (self.received, self.answered);
        let inverted = |i: Interval| i.earliest > i.latest;
        let back = received.earliest > answered.latest;
        if inverted(received) || inverted(answered) || back {
            return None;
        }
        let earliest = received.earliest.max(answered.earliest);
        let latest = received.latest.min(answered.latest);
        let round_trip = self.returned.0.checked_sub(self.sent.0)?;
        let offset = |mesh: i64, local: u64| i128::from(mesh) - i128::from(local);
        let low = offset(earliest.nanos(), self.returned.0);
        let high =
            offset(latest.nanos(), self.sent.0) + i128::from(drift.over(round_trip));
        (low <= high).then(|| Measurement::between(self.returned, low, high))
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Interval, Monotonic, Stamp};

    use super::Exchange;
    use crate::Drift;
    use crate::measurement::MAX_ERROR;

    const SECOND_NS: u64 = 1_000_000_000;

    /// A peer interval from `earliest` to `latest`.
    fn peer(earliest: i64, latest: i64) -> Interval {
        Interval {
            earliest: Stamp::from_nanos(earliest),
            latest: Stamp::from_nanos(latest),
        }
    }

    fn drift(ppb: u32) -> Drift {
        Drift::from_ppb(ppb).expect("valid")
    }

    /// The exchange as `(offset, error)`, with `ppb` of drift.
    fn check(
        ppb: u32,
        sent: u64,
        received: Interval,
        answered: Interval,
        returned: u64,
    ) -> Option<(i64, i64)> {
        let exchange = Exchange {
            sent: Monotonic(sent),
            received,
            answered,
            returned: Monotonic(returned),
        };
        let m = exchange.measure(drift(ppb));
        m.map(|m| (m.offset().nanos(), m.error().nanos()))
    }

    mod known_exchanges {
        use super::*;

        #[test]
        fn halves_a_symmetric_round_trip() {
            let (received, answered) = (peer(6_100, 6_100), peer(6_150, 6_150));
            assert_eq!(
                check(0, 1_000, received, answered, 1_250),
                Some((5_000, 100))
            );
        }

        #[test]
        fn covers_an_asymmetric_round_trip() {
            let (received, answered) = (peer(6_010, 6_010), peer(6_060, 6_060));
            assert_eq!(
                check(0, 1_000, received, answered, 1_250),
                Some((4_910, 100))
            );
        }

        #[test]
        fn uses_the_late_edge_of_arrival_and_the_early_edge_of_answer() {
            let (received, answered) = (peer(6_000, 6_120), peer(6_140, 6_400));
            assert_eq!(
                check(0, 1_000, received, answered, 1_250),
                Some((5_005, 115))
            );
        }

        #[test]
        fn bounds_each_edge_by_both_intervals() {
            let (wide, narrow) = (peer(5_000, 7_000), peer(6_150, 6_150));
            assert_eq!(check(0, 1_000, wide, narrow, 1_250), Some((5_025, 125)));
            let narrow = peer(6_100, 6_100);
            assert_eq!(check(0, 1_000, narrow, wide, 1_250), Some((4_975, 125)));
        }

        #[test]
        fn gives_the_peer_interval_for_a_zero_round_trip() {
            let interval = peer(5_990, 6_010);
            assert_eq!(
                check(0, 1_000, interval, interval, 1_000),
                Some((5_000, 10))
            );
        }

        #[test]
        fn widens_only_the_high_edge_by_drift() {
            let (instant, returned) = (peer(0, 0), 2 * SECOND_NS);
            let exact = check(0, SECOND_NS, instant, instant, returned);
            assert_eq!(exact, Some((-1_500_000_000, 500_000_000)));
            let drifted = check(1_000, SECOND_NS, instant, instant, returned);
            assert_eq!(drifted, Some((-1_499_999_500, 500_000_500)));
        }

        #[test]
        fn is_unknown_when_the_error_passes_36500_days() {
            let widest = MAX_ERROR.nanos();
            let interval = peer(-widest, widest);
            assert_eq!(check(0, 0, interval, interval, 0), Some((0, widest)));
            let interval = peer(-widest - 5, widest + 1);
            assert_eq!(check(0, 0, interval, interval, 0), Some((-2, widest)));
        }

        #[test]
        fn is_unknown_when_the_round_trip_passes_36500_days() {
            let widest = MAX_ERROR.nanos();
            let interval = peer(-widest, widest - 2);
            assert_eq!(check(0, 0, interval, interval, 2), Some((-2, widest)));
            assert_eq!(check(0, 0, interval, interval, 7), Some((-5, widest)));
        }

        #[test]
        fn is_unknown_when_the_error_passes_a_span() {
            let widest = peer(i64::MIN, i64::MAX);
            let unknown = (-1, MAX_ERROR.nanos());
            assert_eq!(check(0, 0, widest, widest, 0), Some(unknown));
        }

        #[test]
        fn stops_the_offset_at_the_last_span_and_covers_the_cut() {
            let instant = peer(i64::MIN, i64::MIN);
            assert_eq!(check(0, 7, instant, instant, 7), Some((i64::MIN, 7)));
        }
    }

    mod when_crossed {
        use super::*;

        #[test]
        fn fails_when_the_peer_answers_too_late() {
            let (received, answered) = (peer(6_100, 6_100), peer(6_351, 6_351));
            assert_eq!(check(0, 1_000, received, answered, 1_250), None);
            let most = peer(6_350, 6_350);
            let ok = check(0, 1_000, received, most, 1_250);
            assert_eq!(ok, Some((5_100, 0)));
        }

        #[test]
        fn fails_when_drift_cannot_cover_the_gap() {
            let received = peer(0, 0);
            let most = peer(1_000_001_000, 1_000_001_000);
            assert_eq!(check(1_000, 0, received, most, SECOND_NS), Some((1_000, 0)));
            let past = peer(1_000_001_001, 1_000_001_001);
            assert_eq!(check(1_000, 0, received, past, SECOND_NS), None);
        }

        #[test]
        fn fails_when_a_peer_interval_is_inverted() {
            let (honest, inverted) = (peer(6_150, 6_150), peer(6_100, 5_900));
            assert_eq!(check(0, 1_000, inverted, honest, 1_250), None);
            let (honest, inverted) = (peer(6_000, 6_250), peer(6_250, 6_000));
            assert_eq!(check(0, 1_000, honest, inverted, 1_250), None);
        }

        #[test]
        fn fails_when_peer_time_goes_back() {
            let received = peer(6_100, 6_100);
            let back = check(0, 1_000, received, peer(6_099, 6_099), 1_250);
            assert_eq!(back, None);
            let ok = check(0, 1_000, received, peer(6_100, 6_100), 1_250);
            assert_eq!(ok, Some((4_975, 125)));
        }

        #[test]
        fn fails_when_sent_is_after_returned() {
            let interval = peer(-100, 100);
            assert_eq!(check(0, 1_001, interval, interval, 1_000), None);
        }
    }

    mod properties {
        use proptest::array::{uniform3, uniform4};
        use proptest::prelude::*;

        use super::*;
        use crate::world::{ERROR_NS, TIME_NS, World, world};

        /// The peer's mesh time at local time `t`, inside an interval `below` and
        /// `above` wide around it.
        fn honest(w: World, t: u64, below: i64, above: i64) -> Interval {
            let mesh = i64::try_from(i128::from(t) + w.truth(t)).expect("fits");
            peer(mesh - below, mesh + above)
        }

        /// An exchange sent at `sent` with honest intervals `widths` wide around the
        /// peer's mesh time, and the time it returned.
        fn exchange(
            w: World,
            sent: u64,
            delays: [u64; 3],
            widths: [i64; 4],
        ) -> Exchange {
            let [out, hold, back] = delays;
            let (arrived, left) = (sent + out, sent + out + hold);
            Exchange {
                sent: Monotonic(sent),
                received: honest(w, arrived, widths[0], widths[1]),
                answered: honest(w, left, widths[2], widths[3]),
                returned: Monotonic(left + back),
            }
        }

        proptest! {
            #[test]
            fn holds_the_truth_for_any_delays(
                w in world(),
                sent in 0..TIME_NS,
                delays in uniform3(0..TIME_NS),
                widths in uniform4(0..ERROR_NS),
            ) {
                let exchange = exchange(w, sent, delays, widths);
                let m = exchange.measure(w.drift).expect("an honest exchange");
                let at = exchange.returned.0;
                prop_assert!(w.holds_truth_at(m, at), "{m:?} misses {}", w.truth(at));
            }

            #[test]
            fn holds_the_truth_or_is_unknown_for_wide_intervals(
                w in world(),
                sent in 0..TIME_NS,
                delays in uniform3(0..TIME_NS),
                widths in uniform4(prop_oneof![
                    0..ERROR_NS,
                    2 * MAX_ERROR.nanos()..=2 * MAX_ERROR.nanos() + ERROR_NS,
                ]),
            ) {
                let exchange = exchange(w, sent, delays, widths);
                let at = exchange.returned.0;
                let m = exchange.measure(w.drift).expect("an honest exchange");
                let truth = w.truth(at);
                if m.error() < MAX_ERROR {
                    prop_assert!(w.holds_truth_at(m, at), "{m:?} misses {truth}");
                } else {
                    // At the center of edges that hold the truth, which are at most
                    // the narrower interval, the round trip, and drift apart.
                    let width = |a: i64, b: i64| i128::from(a) + i128::from(b);
                    let narrowest =
                        width(widths[0], widths[1]).min(width(widths[2], widths[3]));
                    let reach = narrowest / 2 + i128::from(at - sent) + 1;
                    let miss = i128::from(m.offset().nanos()) - truth;
                    prop_assert!(miss.abs() <= reach, "{m:?} is {miss} from the truth");
                }
            }

            #[test]
            fn never_panics_at_any_input(
                local in any::<[u64; 2]>(),
                mesh in any::<[i64; 4]>(),
                ppb in 0..=100_000_000_u32,
            ) {
                let exchange = Exchange {
                    sent: Monotonic(local[0]),
                    received: peer(mesh[0], mesh[1]),
                    answered: peer(mesh[2], mesh[3]),
                    returned: Monotonic(local[1]),
                };
                if let Some(m) = exchange.measure(drift(ppb)) {
                    prop_assert_eq!(m.at(), Monotonic(local[1]));
                    prop_assert!(m.error() <= MAX_ERROR);
                }
            }
        }
    }
}
