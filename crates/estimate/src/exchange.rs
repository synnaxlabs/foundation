use types::time::{Interval, Monotonic};

use crate::{Drift, Error, Measurement};

/// One reading of another clock's mesh time, taken between two readings of the local
/// monotonic clock.
///
/// ```
/// use estimate::{Drift, Exchange};
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
/// let m = exchange.measure(Drift::from_ppb(0)?)?;
/// let ns = Span::from_nanos;
/// assert_eq!((m.offset(), m.error()), (ns(5_000), ns(110)));
/// # Ok::<(), estimate::Error>(())
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
    ///
    /// # Errors
    ///
    /// [`Error::Crossed`] when the exchange allows no offset: an interval is inverted,
    /// the other clock goes back, the local clock drifts more than `drift`, or `sent`
    /// is after `returned`.
    pub fn measure(self, drift: Drift) -> Result<Measurement, Error> {
        let (received, answered) = (self.received, self.answered);
        let inverted = |i: Interval| i.earliest > i.latest;
        let back = received.earliest > answered.latest;
        if inverted(received) || inverted(answered) || back {
            return Err(Error::Crossed);
        }
        let earliest = received.earliest.max(answered.earliest);
        let latest = received.latest.min(answered.latest);
        let round_trip = self.returned.0.checked_sub(self.sent.0);
        let round_trip = round_trip.ok_or(Error::Crossed)?;
        let offset = |mesh: i64, local: u64| i128::from(mesh) - i128::from(local);
        let low = offset(earliest.nanos(), self.returned.0);
        let high =
            offset(latest.nanos(), self.sent.0) + i128::from(drift.over(round_trip));
        if low > high {
            return Err(Error::Crossed);
        }
        Ok(Measurement::between(self.returned, low, high))
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Interval, Monotonic, Stamp};

    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Error, Exchange};

    const SECOND_NS: u64 = 1_000_000_000;

    /// A peer interval from `earliest` to `latest`.
    fn peer(earliest: i64, latest: i64) -> Interval {
        Interval {
            earliest: Stamp::from_nanos(earliest),
            latest: Stamp::from_nanos(latest),
        }
    }

    /// The exchange as `(offset, error)`, with `ppb` of drift.
    fn check(
        ppb: u32,
        sent: u64,
        received: Interval,
        answered: Interval,
        returned: u64,
    ) -> Result<(i64, i64), Error> {
        let exchange = Exchange {
            sent: Monotonic(sent),
            received,
            answered,
            returned: Monotonic(returned),
        };
        let m = exchange.measure(Drift::from_ppb(ppb)?)?;
        Ok((m.offset().nanos(), m.error().nanos()))
    }

    mod known_exchanges {
        use super::*;

        #[test]
        fn halves_a_symmetric_round_trip() {
            let (received, answered) = (peer(6_100, 6_100), peer(6_150, 6_150));
            assert_eq!(check(0, 1_000, received, answered, 1_250), Ok((5_000, 100)));
        }

        #[test]
        fn covers_an_asymmetric_round_trip() {
            let (received, answered) = (peer(6_010, 6_010), peer(6_060, 6_060));
            assert_eq!(check(0, 1_000, received, answered, 1_250), Ok((4_910, 100)));
        }

        #[test]
        fn uses_the_late_edge_of_arrival_and_the_early_edge_of_answer() {
            let (received, answered) = (peer(6_000, 6_120), peer(6_140, 6_400));
            assert_eq!(check(0, 1_000, received, answered, 1_250), Ok((5_005, 115)));
        }

        #[test]
        fn bounds_each_edge_by_both_intervals() {
            let (wide, narrow) = (peer(5_000, 7_000), peer(6_150, 6_150));
            assert_eq!(check(0, 1_000, wide, narrow, 1_250), Ok((5_025, 125)));
            let narrow = peer(6_100, 6_100);
            assert_eq!(check(0, 1_000, narrow, wide, 1_250), Ok((4_975, 125)));
        }

        #[test]
        fn gives_the_peer_interval_for_a_zero_round_trip() {
            let interval = peer(5_990, 6_010);
            assert_eq!(check(0, 1_000, interval, interval, 1_000), Ok((5_000, 10)));
        }

        #[test]
        fn widens_only_the_high_edge_by_drift() {
            let (instant, returned) = (peer(0, 0), 2 * SECOND_NS);
            let exact = check(0, SECOND_NS, instant, instant, returned);
            assert_eq!(exact, Ok((-1_500_000_000, 500_000_000)));
            let drifted = check(1_000, SECOND_NS, instant, instant, returned);
            assert_eq!(drifted, Ok((-1_499_999_500, 500_000_500)));
        }

        #[test]
        fn stops_the_error_at_36500_days() {
            let widest = MAX_ERROR.nanos();
            let interval = peer(-widest - 1, widest + 1);
            assert_eq!(check(0, 0, interval, interval, 0), Ok((0, widest)));
        }

        #[test]
        fn stops_an_error_wider_than_a_span_at_36500_days() {
            let all = peer(i64::MIN, i64::MAX);
            assert_eq!(check(0, 0, all, all, 0), Ok((-1, MAX_ERROR.nanos())));
        }
    }

    mod when_crossed {
        use super::*;

        #[test]
        fn fails_when_the_peer_answers_too_late() {
            let (received, answered) = (peer(6_100, 6_100), peer(6_351, 6_351));
            let err = check(0, 1_000, received, answered, 1_250);
            assert_eq!(err, Err(Error::Crossed));
            assert_eq!(Error::Crossed.to_string(), "exchange allows no offset");
            let most = peer(6_350, 6_350);
            let ok = check(0, 1_000, received, most, 1_250);
            assert_eq!(ok, Ok((5_100, 0)));
        }

        #[test]
        fn fails_when_drift_cannot_cover_the_gap() {
            let received = peer(0, 0);
            let most = peer(1_000_001_000, 1_000_001_000);
            assert_eq!(check(1_000, 0, received, most, SECOND_NS), Ok((1_000, 0)));
            let past = peer(1_000_001_001, 1_000_001_001);
            let err = check(1_000, 0, received, past, SECOND_NS);
            assert_eq!(err, Err(Error::Crossed));
        }

        #[test]
        fn fails_when_a_peer_interval_is_inverted() {
            let (honest, inverted) = (peer(6_150, 6_150), peer(6_100, 5_900));
            let err = check(0, 1_000, inverted, honest, 1_250);
            assert_eq!(err, Err(Error::Crossed));
            let (honest, inverted) = (peer(6_000, 6_250), peer(6_250, 6_000));
            let err = check(0, 1_000, honest, inverted, 1_250);
            assert_eq!(err, Err(Error::Crossed));
        }

        #[test]
        fn fails_when_peer_time_goes_back() {
            let received = peer(6_100, 6_100);
            let err = check(0, 1_000, received, peer(6_099, 6_099), 1_250);
            assert_eq!(err, Err(Error::Crossed));
            let ok = check(0, 1_000, received, peer(6_100, 6_100), 1_250);
            assert_eq!(ok, Ok((4_975, 125)));
        }

        #[test]
        fn fails_when_sent_is_after_returned() {
            let interval = peer(-100, 100);
            let err = check(0, 1_001, interval, interval, 1_000);
            assert_eq!(err, Err(Error::Crossed));
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

        proptest! {
            #[test]
            fn holds_the_truth_for_any_delays(
                w in world(),
                sent in 0..TIME_NS,
                delays in uniform3(0..TIME_NS),
                widths in uniform4(0..ERROR_NS),
            ) {
                let [out, hold, back] = delays;
                let (arrived, left) = (sent + out, sent + out + hold);
                let exchange = Exchange {
                    sent: Monotonic(sent),
                    received: honest(w, arrived, widths[0], widths[1]),
                    answered: honest(w, left, widths[2], widths[3]),
                    returned: Monotonic(left + back),
                };
                let m = exchange.measure(w.drift).expect("an honest exchange");
                let at = left + back;
                prop_assert!(w.holds_truth_at(m, at), "{m:?} misses {}", w.truth(at));
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
                match exchange.measure(Drift::from_ppb(ppb).expect("valid")) {
                    Ok(m) => {
                        prop_assert_eq!(m.at(), Monotonic(local[1]));
                        prop_assert!(m.error() <= MAX_ERROR);
                    }
                    Err(Error::Crossed) => {}
                    Err(e @ (Error::Backwards { .. } | Error::Bound { .. }
                        | Error::Disjoint | Error::Drift { .. } | Error::NoSources
                        | Error::NoMajority { .. } | Error::Open)) => {
                        prop_assert!(false, "unexpected {e}");
                    }
                }
            }
        }
    }
}
