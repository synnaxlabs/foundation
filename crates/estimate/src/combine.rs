use types::time::{Monotonic, Span};

use crate::{Drift, Error, Measurement};

/// Combines the best measurement of each source into one estimate at `now`.
///
/// It widens each bound to `now` by `drift`, then finds the offsets inside the most
/// bounds (Marzullo's intersection). The estimate covers all of them, so a tie between
/// groups that disagree gives a wide bound, not a guess. A falseticker, whose bound
/// misses the estimate, does not move it. When all sources agree, the bound is not
/// wider than the narrowest one.
///
/// # Errors
///
/// [`Error::NoSources`] when `measurements` is empty. [`Error::NoMajority`] when no
/// offset is inside the bounds of more than half of the sources.
///
/// ```
/// use estimate::{Drift, Measurement};
/// use types::time::{Monotonic, Span};
///
/// fn ms(n: i64) -> Span {
///     Span::from_nanos(n * 1_000_000)
/// }
/// let source = |offset, error| Measurement::new(Monotonic(0), ms(offset), ms(error));
/// let sources = [source(10, 4)?, source(12, 4)?, source(500, 1)?];
/// let estimate = estimate::combine(Monotonic(0), Drift::default(), &sources)?;
/// assert_eq!((estimate.offset(), estimate.error()), (ms(11), ms(3)));
/// # Ok::<(), estimate::Error>(())
/// ```
pub fn combine(
    now: Monotonic,
    drift: Drift,
    measurements: &[Measurement],
) -> Result<Measurement, Error> {
    let sources = measurements.len();
    if sources == 0 {
        return Err(Error::NoSources);
    }
    let mut edges = Vec::with_capacity(2 * sources);
    for m in measurements {
        let (low, high) = m.bounds_at(now, drift);
        edges.push((low, Edge::Low));
        edges.push((high, Edge::High));
    }
    // A low edge sorts before a high edge at the same offset, so bounds that only
    // touch still share that offset.
    edges.sort_unstable();
    let (agreeing, low, high) = most_covered(&edges);
    if 2 * agreeing <= sources {
        return Err(Error::NoMajority { sources, agreeing });
    }
    let offset = (low + high).div_euclid(2);
    let error = (high - offset).max(offset - low);
    Measurement::new(now, saturate(offset), saturate(error))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Edge {
    Low,
    High,
}

/// The most bounds that share one offset, and the lowest and highest offsets inside
/// that many bounds.
fn most_covered(edges: &[(i128, Edge)]) -> (usize, i128, i128) {
    let (mut covered, mut most, mut low, mut high) = (0, 0, 0, 0);
    for &(offset, edge) in edges {
        match edge {
            Edge::Low => {
                covered += 1;
                if covered > most {
                    (most, low) = (covered, offset);
                }
            }
            Edge::High => {
                if covered == most {
                    high = offset;
                }
                covered -= 1;
            }
        }
    }
    (most, low, high)
}

fn saturate(nanos: i128) -> Span {
    let clamped = nanos.clamp(i64::MIN.into(), i64::MAX.into());
    Span::from_nanos(i64::try_from(clamped).expect("invariant: clamped to i64"))
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::{Drift, Error, Measurement, combine};

    const NO_DRIFT: Drift = Drift::from_ppb(0);

    fn source(offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(0), offset, error).expect("valid")
    }

    /// Combines at the sources' own time with no drift.
    fn check(sources: &[Measurement]) -> Result<(i64, i64), Error> {
        let m = combine(Monotonic(0), NO_DRIFT, sources)?;
        Ok((m.offset().nanos(), m.error().nanos()))
    }

    mod when_sources_agree {
        use super::*;

        #[test]
        fn gives_one_source_as_it_is() {
            assert_eq!(check(&[source(7, 3)]), Ok((7, 3)));
        }

        #[test]
        fn gives_the_intersection() {
            assert_eq!(check(&[source(10, 10), source(20, 10)]), Ok((15, 5)));
        }

        #[test]
        fn counts_bounds_that_only_touch() {
            assert_eq!(check(&[source(5, 5), source(15, 5)]), Ok((10, 0)));
        }

        #[test]
        fn rounds_an_odd_width_outward() {
            assert_eq!(check(&[source(0, 1), source(2, 2)]), Ok((0, 1)));
        }

        #[test]
        fn widens_each_bound_to_now() {
            let old = Measurement::new(Monotonic(0), Span::ZERO, Span::ZERO);
            let old = old.expect("valid");
            let now = Monotonic(1_000_000_000);
            let m = combine(now, Drift::from_ppb(1_000), &[old]).expect("one source");
            assert_eq!(
                (m.at(), m.offset(), m.error()),
                (now, Span::ZERO, Span::MICROSECOND)
            );
        }
    }

    mod when_sources_disagree {
        use super::*;

        #[test]
        fn drops_a_falseticker() {
            let sources = [source(10, 4), source(500, 1), source(12, 4)];
            assert_eq!(check(&sources), Ok((11, 3)));
        }

        #[test]
        fn covers_both_groups_of_a_tie() {
            let sources = [source(5, 5), source(0, 1), source(10, 1)];
            assert_eq!(check(&sources), Ok((5, 5)));
        }

        #[test]
        fn fails_with_no_majority() {
            let err = check(&[source(0, 1), source(10, 1)]);
            assert_eq!(
                err,
                Err(Error::NoMajority {
                    sources: 2,
                    agreeing: 1
                })
            );
        }

        #[test]
        fn fails_with_an_even_split() {
            let sources = [source(0, 1), source(0, 1), source(10, 1), source(10, 1)];
            let err = check(&sources);
            assert_eq!(
                err,
                Err(Error::NoMajority {
                    sources: 4,
                    agreeing: 2
                })
            );
        }
    }

    #[test]
    fn fails_with_no_sources() {
        assert_eq!(check(&[]), Err(Error::NoSources));
        assert_eq!(Error::NoSources.to_string(), "no time sources to combine");
    }

    #[test]
    fn saturates_at_extreme_offsets_and_errors() {
        let most = Measurement::new(
            Monotonic(0),
            Span::from_nanos(i64::MAX),
            Span::from_nanos(i64::MAX),
        );
        let least =
            Measurement::new(Monotonic(0), Span::from_nanos(i64::MIN), Span::ZERO);
        let sources = [most.expect("valid"), least.expect("valid")];
        let m = combine(Monotonic(u64::MAX), Drift::from_ppb(u32::MAX), &sources);
        let m = m.expect("both bounds cover zero");
        assert_eq!(m.error(), Span::from_nanos(i64::MAX));
    }

    mod properties {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        const TIME_NS: u64 = 1 << 40;
        const ERROR_NS: i64 = 1 << 30;
        const OFFSET_NS: i64 = 1 << 60;

        /// A true offset that starts at `start` at monotonic zero and moves at `rate`
        /// parts per billion, never faster than `drift`.
        #[derive(Clone, Copy, Debug)]
        struct World {
            drift: Drift,
            rate: i64,
            start: i64,
            now: u64,
        }

        impl World {
            fn truth(self, t: u64) -> i128 {
                let moved =
                    (i128::from(self.rate) * i128::from(t)).div_euclid(1_000_000_000);
                i128::from(self.start) + moved
            }

            fn holds_truth(self, m: Measurement) -> bool {
                let miss = i128::from(m.offset().nanos()) - self.truth(m.at().0);
                miss.abs() <= i128::from(m.error().nanos())
            }
        }

        fn nanos(n: i128) -> Span {
            Span::from_nanos(i64::try_from(n).expect("test values fit in i64"))
        }

        fn world() -> impl Strategy<Value = World> {
            (0..=1_000_000_u32, -OFFSET_NS..OFFSET_NS, 0..TIME_NS).prop_flat_map(
                |(ppb, start, now)| {
                    let most = i64::from(ppb);
                    (-most..=most).prop_map(move |rate| World {
                        drift: Drift::from_ppb(ppb),
                        rate,
                        start,
                        now,
                    })
                },
            )
        }

        /// A source whose bound holds the true offset at its own time.
        fn truechimer(world: World) -> impl Strategy<Value = Measurement> {
            (0..TIME_NS, 0..ERROR_NS).prop_flat_map(move |(at, error)| {
                (-error..=error).prop_map(move |slack| {
                    let offset = nanos(world.truth(at) + i128::from(slack));
                    let m = Measurement::new(
                        Monotonic(at),
                        offset,
                        Span::from_nanos(error),
                    );
                    m.expect("valid")
                })
            })
        }

        /// A source at `now` whose bound misses every offset from `low` to `high`.
        fn falseticker(
            now: u64,
            low: i128,
            high: i128,
        ) -> impl Strategy<Value = Measurement> {
            (any::<bool>(), 1..ERROR_NS, 0..ERROR_NS).prop_map(
                move |(above, gap, error)| {
                    let reach = i128::from(gap) + i128::from(error);
                    let offset = if above { high + reach } else { low - reach };
                    let m = Measurement::new(
                        Monotonic(now),
                        nanos(offset),
                        Span::from_nanos(error),
                    );
                    m.expect("valid")
                },
            )
        }

        fn agreeing() -> impl Strategy<Value = (World, Vec<Measurement>)> {
            world().prop_flat_map(|w| (Just(w), vec(truechimer(w), 1..10)))
        }

        /// Truechimers and fewer falsetickers, each disjoint from every truechimer.
        fn disagreeing() -> impl Strategy<Value = (World, Vec<Measurement>)> {
            agreeing()
                .prop_flat_map(|(w, chimers)| {
                    let now = Monotonic(w.now);
                    let bounds = chimers.iter().map(|m| m.bounds_at(now, w.drift));
                    let (low, high) = bounds
                        .reduce(|(l, h), (l2, h2)| (l.min(l2), h.max(h2)))
                        .expect("at least one truechimer");
                    let liars = vec(falseticker(w.now, low, high), 0..chimers.len());
                    (Just(w), Just(chimers), liars)
                })
                .prop_flat_map(|(w, mut sources, liars)| {
                    sources.extend(liars);
                    (Just(w), Just(sources).prop_shuffle())
                })
        }

        fn any_sources() -> impl Strategy<Value = Vec<Measurement>> {
            let one = (0..TIME_NS, -ERROR_NS..ERROR_NS, 0..ERROR_NS).prop_map(
                |(at, o, e)| {
                    let m = Measurement::new(
                        Monotonic(at),
                        Span::from_nanos(o),
                        Span::from_nanos(e),
                    );
                    m.expect("valid")
                },
            );
            vec(one, 0..10)
        }

        proptest! {
            #[test]
            fn holds_the_truth_when_every_source_does((w, sources) in agreeing()) {
                let m = combine(Monotonic(w.now), w.drift, &sources);
                let m = m.expect("every bound holds the truth at now");
                prop_assert!(w.holds_truth(m), "{m:?} misses {}", w.truth(w.now));
            }

            #[test]
            fn is_no_wider_than_the_narrowest_source((w, sources) in agreeing()) {
                let now = Monotonic(w.now);
                let m = combine(now, w.drift, &sources).expect("sources agree");
                let narrowest = sources.iter().map(|s| s.error_at(now, w.drift)).min();
                prop_assert!(Some(m.error()) <= narrowest, "{m:?} vs {narrowest:?}");
            }

            #[test]
            fn holds_the_truth_despite_falsetickers((w, sources) in disagreeing()) {
                let m = combine(Monotonic(w.now), w.drift, &sources);
                let m = m.expect("truechimers are a majority");
                prop_assert!(w.holds_truth(m), "{m:?} misses {}", w.truth(w.now));
            }

            #[test]
            fn ignores_input_order(
                (sources, shuffled) in any_sources()
                    .prop_flat_map(|s| (Just(s.clone()), Just(s).prop_shuffle())),
                now in 0..TIME_NS,
                ppb in 0..=1_000_000_u32,
            ) {
                let (now, drift) = (Monotonic(now), Drift::from_ppb(ppb));
                let given = combine(now, drift, &sources);
                prop_assert_eq!(given, combine(now, drift, &shuffled));
            }
        }
    }
}
