//! One estimate from the best measurement of each source.

use std::fmt;

use types::time::Monotonic;

use crate::measurement::MAX_ERROR;
use crate::{Drift, Filter, Measurement};

/// Combines the best measurement of each source into one estimate at `now`.
///
/// It widens each bound to `now` by `drift`, then finds the offsets inside the most
/// bounds (Marzullo's intersection). The estimate covers all of them, so a tie between
/// groups that disagree gives a wide bound, not a guess. When all sources agree, the
/// bound is not wider than the narrowest one. The estimate holds the true offset when
/// the bounds that hold it are a majority and every other bound misses them all. A
/// falseticker that overlaps some of them can move it, as in NTP.
///
/// A bound of 36500 days at `now` is unknown. It votes only when no bound is known, so
/// it never turns a split into an estimate.
///
/// # Errors
///
/// - [`Error::NoSources`] when no filter holds a measurement.
/// - [`Error::NoMajority`] when no offset is inside more than half of the bounds that
///   vote.
///
/// ```
/// use estimate::combine::combine;
/// use estimate::{Drift, Filter, Measurement};
/// use types::time::{Monotonic, Span};
///
/// fn ms(n: i64) -> Span {
///     Span::from_nanos(n * 1_000_000)
/// }
/// let mut sources = [Filter::default(), Filter::default(), Filter::default()];
/// let readings = [(10, 4), (12, 4), (500, 1)];
/// for (filter, (offset, error)) in sources.iter_mut().zip(readings) {
///     let m = Measurement::new(Monotonic(0), ms(offset), ms(error));
///     filter.push(m.expect("at most 36500 days"));
/// }
/// let estimate = combine(Monotonic(0), Drift::UNDISCIPLINED, &sources)?;
/// assert_eq!((estimate.offset(), estimate.error()), (ms(11), ms(3)));
/// # Ok::<(), estimate::combine::Error>(())
/// ```
pub fn combine<'a>(
    now: Monotonic,
    drift: Drift,
    sources: impl IntoIterator<Item = &'a Filter>,
) -> Result<Measurement, Error> {
    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    for m in sources.into_iter().filter_map(|s| s.best(now, drift)) {
        let edges = if m.error_at(now, drift) < MAX_ERROR {
            &mut known
        } else {
            &mut unknown
        };
        let (low, high) = m.bounds_at(now, drift);
        edges.extend([(low, Edge::Low), (high, Edge::High)]);
    }
    let mut edges = if known.is_empty() { unknown } else { known };
    let sources = edges.len() / 2;
    if sources == 0 {
        return Err(Error::NoSources);
    }
    // A low edge sorts before a high edge at the same offset, so bounds that only
    // touch still share that offset.
    edges.sort_unstable();
    let (agreeing, low, high) = most_covered(&edges);
    if 2 * agreeing <= sources {
        return Err(Error::NoMajority { sources, agreeing });
    }
    Ok(Measurement::between(now, low, high))
}

/// Why [`combine`] gave no estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No filter holds a measurement.
    NoSources,
    /// No offset is inside the bounds of more than half of the sources that vote.
    NoMajority {
        /// The number of sources that vote.
        sources: usize,
        /// The most sources whose bounds share an offset.
        agreeing: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSources => f.write_str("no time sources to combine"),
            Self::NoMajority { sources, agreeing } => write!(
                f,
                "no majority of time sources agree: at most {agreeing} of {sources}"
            ),
        }
    }
}

impl std::error::Error for Error {}

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

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use super::{Error, combine};
    use crate::measurement::MAX_ERROR;
    use crate::{Drift, Filter, Measurement};

    fn drift(ppb: u32) -> Drift {
        Drift::from_ppb(ppb).expect("valid")
    }

    fn source(offset: i64, error: i64) -> Measurement {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(0), offset, error).expect("valid")
    }

    /// One filter per measurement.
    fn filters(measurements: &[Measurement]) -> Vec<Filter> {
        let filter = |&m| {
            let mut f = Filter::default();
            f.push(m);
            f
        };
        measurements.iter().map(filter).collect()
    }

    /// Combines one source per measurement at their own time with no drift.
    fn check(sources: &[Measurement]) -> Result<(i64, i64), Error> {
        let m = combine(Monotonic(0), drift(0), &filters(sources))?;
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
            assert_eq!(check(&[source(-2, 2), source(-1, 2)]), Ok((-2, 2)));
        }

        #[test]
        fn widens_each_bound_to_now() {
            let now = Monotonic(1_000_000_000);
            let m = combine(now, drift(1_000), &filters(&[source(0, 0)]));
            let m = m.expect("one source");
            assert_eq!(
                (m.at(), m.offset(), m.error()),
                (now, Span::ZERO, Span::MICROSECOND)
            );
        }

        #[test]
        fn keeps_offsets_at_both_ends_of_i64() {
            assert_eq!(check(&[source(i64::MAX, 0)]), Ok((i64::MAX, 0)));
            let widest = MAX_ERROR.nanos();
            let both = [source(i64::MIN, widest), source(i64::MIN, widest)];
            assert_eq!(check(&both), Ok((i64::MIN, widest)));
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

        /// Three of five bounds hold 0, but two falsetickers overlap two of them, so
        /// four bounds share 4 to 6. Marzullo has this limit.
        #[test]
        fn follows_falsetickers_that_overlap_truechimers() {
            let truechimers = [source(4, 5), source(4, 5), source(0, 1)];
            let sources = [truechimers.as_slice(), &[source(5, 1), source(5, 1)]];
            assert_eq!(check(&sources.concat()), Ok((5, 1)));
        }

        #[test]
        fn fails_with_no_majority() {
            let err = check(&[source(0, 1), source(10, 1)]);
            let split = Error::NoMajority {
                sources: 2,
                agreeing: 1,
            };
            assert_eq!(err, Err(split));
            assert_eq!(
                split.to_string(),
                "no majority of time sources agree: at most 1 of 2"
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

    mod filters {
        use super::*;

        #[test]
        fn uses_the_best_of_each() {
            let mut one = Filter::default();
            one.push(source(0, 100));
            one.push(source(50, 1));
            let m = combine(Monotonic(0), drift(0), [&one]).expect("one source");
            assert_eq!((m.offset().nanos(), m.error().nanos()), (50, 1));
        }

        #[test]
        fn counts_each_once() {
            let mut busy = Filter::default();
            for _ in 0..3 {
                busy.push(source(0, 1));
            }
            let quiet = filters(&[source(10, 1), source(10, 1)]);
            let m = combine(Monotonic(0), drift(0), quiet.iter().chain([&busy]));
            let m = m.expect("two of three agree");
            assert_eq!((m.offset().nanos(), m.error().nanos()), (10, 1));
        }

        #[test]
        fn skips_empty_ones() {
            let mut sources = filters(&[source(7, 3)]);
            sources.push(Filter::default());
            let m = combine(Monotonic(0), drift(0), &sources).expect("one source");
            assert_eq!((m.offset().nanos(), m.error().nanos()), (7, 3));
        }

        #[test]
        fn fails_with_none_holding_a_measurement() {
            let empty = [Filter::default()];
            let err = combine(Monotonic(0), drift(0), &empty);
            assert_eq!(err, Err(Error::NoSources));
            assert_eq!(Error::NoSources.to_string(), "no time sources to combine");
        }
    }

    mod when_a_bound_is_unknown {
        use super::*;

        #[test]
        fn leaves_a_split_without_a_majority() {
            let os = source(0, MAX_ERROR.nanos());
            let sources = [source(0, 1_000), source(10_000_000_000, 1_000), os];
            let split = Error::NoMajority {
                sources: 2,
                agreeing: 1,
            };
            assert_eq!(check(&sources), Err(split));
        }

        /// The old bound grows to 36500 days at `now`, so the known bound it misses
        /// wins.
        #[test]
        fn drops_a_bound_grown_to_36500_days() {
            let widest = MAX_ERROR.nanos();
            let now = Monotonic(1_000_000_000);
            let offset = Span::from_nanos(widest + 1_000);
            let known = Measurement::new(now, offset, Span::from_nanos(1));
            let sources = filters(&[source(0, widest - 500), known.expect("valid")]);
            let m = combine(now, drift(1_000), &sources).expect("one known source");
            assert_eq!((m.offset(), m.error().nanos()), (offset, 1));
        }

        #[test]
        fn gives_an_estimate_with_no_known_bound() {
            let sources = filters(&[source(0, MAX_ERROR.nanos())]);
            let m = combine(Monotonic(1_000_000_000), drift(1_000), &sources);
            let m = m.expect("an unknown source still gives an estimate");
            assert_eq!((m.offset(), m.error()), (Span::ZERO, MAX_ERROR));
        }

        /// The old bound grows past 36500 days to hold the true offset of 36500 days
        /// and 1 ns, so it must not cut the new bound.
        #[test]
        fn keeps_a_bound_grown_past_36500_days() {
            let widest = MAX_ERROR.nanos();
            let now = Monotonic(1_000_000_000);
            let new = Measurement::new(now, Span::from_nanos(widest), MAX_ERROR);
            let sources = filters(&[source(0, widest - 500), new.expect("valid")]);
            let m = combine(now, drift(1_000), &sources).expect("both hold the truth");
            let half = widest / 2 + 250;
            assert_eq!((m.offset().nanos(), m.error().nanos()), (half, half));
        }
    }

    mod properties {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        use crate::world::{
            ERROR_NS, TIME_NS, World, agreeing, any_measurements, nanos, world,
        };

        impl World {
            fn combine(self, sources: &[Measurement]) -> Result<Measurement, Error> {
                combine(Monotonic(self.now), self.drift, &filters(sources))
            }
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
                    let offset = nanos(if above { high + reach } else { low - reach });
                    let error = Span::from_nanos(error);
                    Measurement::new(Monotonic(now), offset, error).expect("valid")
                },
            )
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

        /// Sources with errors near 36500 days that hold the true offset at an edge,
        /// so drift can carry it past 36500 days.
        fn wide() -> impl Strategy<Value = (World, Vec<Measurement>)> {
            world().prop_flat_map(|w| {
                let widest = MAX_ERROR.nanos();
                let errors = widest - ERROR_NS..=widest;
                let one = (0..TIME_NS, errors, any::<bool>()).prop_map(
                    move |(at, error, above)| {
                        let slack = if above { error } else { -error };
                        let offset = nanos(w.truth(at) + i128::from(slack));
                        let error = Span::from_nanos(error);
                        Measurement::new(Monotonic(at), offset, error).expect("valid")
                    },
                );
                (Just(w), vec(one, 1..10))
            })
        }

        proptest! {
            #[test]
            fn holds_the_truth_when_every_source_does((w, sources) in agreeing()) {
                let m = w.combine(&sources).expect("every bound holds the truth");
                let truth = w.truth(w.now);
                prop_assert!(w.holds_truth_at(m, w.now), "{m:?} misses {truth}");
            }

            #[test]
            fn is_no_wider_than_the_narrowest_source((w, sources) in agreeing()) {
                let m = w.combine(&sources).expect("sources agree");
                let now = Monotonic(w.now);
                let narrowest = sources.iter().map(|s| s.error_at(now, w.drift)).min();
                prop_assert!(Some(m.error()) <= narrowest, "{m:?} vs {narrowest:?}");
            }

            #[test]
            fn holds_the_truth_despite_falsetickers((w, sources) in disagreeing()) {
                let m = w.combine(&sources).expect("truechimers are a majority");
                let truth = w.truth(w.now);
                prop_assert!(w.holds_truth_at(m, w.now), "{m:?} misses {truth}");
            }

            #[test]
            fn holds_the_truth_or_is_unknown((w, sources) in wide()) {
                let m = w.combine(&sources).expect("every bound holds the truth");
                let truth = w.truth(w.now);
                prop_assert!(
                    m.error() == MAX_ERROR || w.holds_truth_at(m, w.now),
                    "{m:?} misses {truth}"
                );
            }

            #[test]
            fn ignores_input_order(
                (sources, shuffled) in any_measurements(ERROR_NS, ERROR_NS)
                    .prop_flat_map(|s| (Just(s.clone()), Just(s).prop_shuffle())),
                now in any::<u64>(),
                ppb in 0..=1_000_000_u32,
            ) {
                let (now, drift) = (Monotonic(now), drift(ppb));
                let given = combine(now, drift, &filters(&sources));
                prop_assert_eq!(given, combine(now, drift, &filters(&shuffled)));
            }

            #[test]
            fn ignores_unknown_bounds_beside_a_known_one(
                known in any_measurements(i64::MAX, ERROR_NS),
                unknown in vec((any::<u64>(), any::<i64>()), 1..4),
                now in any::<u64>(),
                ppb in 0..=1_000_000_u32,
            ) {
                let unknown = unknown.into_iter().map(|(at, offset)| {
                    let offset = Span::from_nanos(offset);
                    Measurement::new(Monotonic(at), offset, MAX_ERROR).expect("valid")
                });
                let all: Vec<_> = known.iter().copied().chain(unknown).collect();
                let (now, drift) = (Monotonic(now), drift(ppb));
                let given = combine(now, drift, &filters(&all));
                prop_assert_eq!(given, combine(now, drift, &filters(&known)));
            }

            #[test]
            fn never_panics_at_any_input(
                sources in any_measurements(i64::MAX, MAX_ERROR.nanos()),
                now in any::<u64>(),
                ppb in 0..=100_000_000_u32,
            ) {
                match combine(Monotonic(now), drift(ppb), &filters(&sources)) {
                    Ok(m) => prop_assert!(m.error() <= MAX_ERROR),
                    Err(Error::NoMajority { .. }) => {}
                    Err(e @ Error::NoSources) => prop_assert!(false, "unexpected {e}"),
                }
            }
        }
    }
}
