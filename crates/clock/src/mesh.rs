use estimate::combine::{self, combine};
use estimate::discipline::{Cause, Discipline};
use estimate::{Filter, Measurement, Slew};
use types::hash::Map;
use types::time::{Interval, Monotonic, Span};

use crate::{DRIFT, source};

/// The words of the cell that [`Reader`]s read: a [`Discipline`] as its kind, a
/// [`Slew`], a [`combine::Error`], and the first estimate.
const WORDS: usize = 12;

/// The mesh clock of one node. It lives on one shard, and [`Reader`]s read it from
/// any.
#[derive(Debug)]
pub struct Clock {
    monotonic: env::clock::Clock,
    sources: Map<source::Key, Filter>,
    next: u64,
    discipline: Discipline,
    /// The target of the first slew.
    first: Option<Measurement>,
    cell: ring::latest::Writer<WORDS>,
}

impl Clock {
    /// Makes a clock with no sources, and a reader for it. Mesh time runs on
    /// `monotonic`, the node's monotonic clock. Readers have no time until a majority
    /// of the sources first agree.
    #[must_use]
    pub fn new(monotonic: env::clock::Clock) -> (Self, Reader) {
        let discipline = Discipline::Unsynced(combine::Error::NoSources);
        let (cell, cell_reader) = ring::latest::new(encode(discipline, None));
        let reader = Reader {
            monotonic: monotonic.clone(),
            cell: cell_reader,
        };
        let clock = Self {
            monotonic,
            sources: Map::default(),
            next: 0,
            discipline,
            first: None,
            cell,
        };
        (clock, reader)
    }

    /// Adds a source with no measurements, then moves mesh time toward the sources,
    /// and returns its key. The source counts against a majority until it pushes.
    pub fn add(&mut self) -> source::Key {
        let key = source::Key(self.next);
        self.next += 1;
        self.sources.insert(key, Filter::default());
        self.steer();
        key
    }

    /// Removes a source and its measurements, then moves mesh time toward the sources
    /// left.
    ///
    /// # Panics
    ///
    /// When `source` was removed already.
    pub fn remove(&mut self, source: source::Key) {
        let removed = self.sources.remove(&source);
        assert!(removed.is_some(), "{source:?} was removed");
        self.steer();
    }

    /// Records a measurement of the monotonic clock from `source`, then moves mesh
    /// time toward the estimate of all sources.
    ///
    /// # Panics
    ///
    /// When `source` was removed.
    pub fn push(&mut self, source: source::Key, measurement: Measurement) {
        let Some(filter) = self.sources.get_mut(&source) else {
            panic!("{source:?} was removed");
        };
        filter.push(measurement);
        self.steer();
    }

    /// Feeds the clock from the node's time sources, today the OS clock `wall`. Each
    /// source measures when its adapter decides: the OS clock at once, then a second
    /// after the last. It never returns; drop the future to stop it.
    ///
    /// # Panics
    ///
    /// When the clock has a source already, because `run` owns every source. On a
    /// thread that `env` did not start. When the OS bound is negative, or the
    /// monotonic clock goes back.
    pub async fn run(mut self, wall: env::wall::Wall) -> ! {
        assert!(self.sources.is_empty(), "a source was added before run");
        let mut wall = source::Wall::new(wall, self.monotonic.clone());
        let source = self.add();
        loop {
            let measurement = wall.next().await;
            self.push(source, measurement);
        }
    }

    fn steer(&mut self) {
        let estimate = combine(self.monotonic.now(), DRIFT, self.sources.values());
        // A write that changes nothing makes the reads that overlap it run again.
        let Some(change) = self.discipline.next(estimate, DRIFT) else {
            return;
        };
        // A slew keeps mesh time from going back only against reads before its `now`,
        // so the clock reads inside the update.
        self.cell.update(|_| {
            self.discipline = change.at(self.monotonic.now(), DRIFT);
            self.first = self
                .first
                .or(self.discipline.slew().map(|slew| slew.target));
            encode(self.discipline, self.first)
        });
    }
}

/// What a clock follows after its last [`Clock::add`], [`Clock::remove`], or
/// [`Clock::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No majority of the sources has agreed yet, so readers have no time. A source
    /// with no measurement counts against a majority. Holds why.
    Unsynced(combine::Error),
    /// A majority of the sources agrees. Holds mesh time now, as an offset from the
    /// monotonic clock with its error. The error can be unknown
    /// ([`Measurement::unknown`]).
    Synced(Measurement),
    /// Mesh time keeps its slew, and the error of the slew's target grows by drift.
    /// Holds mesh time now and why.
    Holdover(Measurement, Cause),
}

/// Reads mesh time from any thread with no lock. Clones read the same clock.
#[derive(Clone, Debug)]
pub struct Reader {
    monotonic: env::clock::Clock,
    cell: ring::latest::Reader<WORDS>,
}

impl Reader {
    /// Mesh time now. The true time is inside the interval. Its midpoint is the
    /// clock's best guess, and it never goes back: a call that starts after another
    /// returns gives a midpoint no earlier, while both edges fit a stamp (to 2162 with
    /// an unknown error). `None` until a majority of the clock's sources first agree.
    #[must_use]
    pub fn now(&self) -> Option<Interval> {
        self.cell.read(|words| {
            let slew = decode(words).slew()?;
            Some(slew.at(self.monotonic.now(), DRIFT).interval())
        })
    }

    /// What the clock follows now, with mesh time at the call. It holds the result of
    /// the last [`Clock::add`], [`Clock::remove`], or [`Clock::push`] to return. That
    /// mesh time is the one [`Reader::now`] gives, so it never goes back.
    /// [`Status::Unsynced`] until a majority of the sources first agree.
    #[must_use]
    pub fn status(&self) -> Status {
        self.cell.read(|words| {
            let at = |slew: Slew| slew.at(self.monotonic.now(), DRIFT);
            match decode(words) {
                Discipline::Unsynced(error) => Status::Unsynced(error),
                Discipline::Synced(slew) => Status::Synced(at(slew)),
                Discipline::Holdover(slew, cause) => Status::Holdover(at(slew), cause),
            }
        })
    }

    /// The clock's first estimate at `reading`, a reading of the node's monotonic
    /// clock: its offset, with its error grown by drift to `reading` (200 ppm, 0.72 s
    /// in one hour). Later estimates never change it. It stamps a reading taken
    /// before a call to [`Reader::now`] that gave `None`: its midpoint is then never
    /// later than the midpoint of an interval from [`Reader::now`]. `None` until a
    /// majority of the clock's sources first agree.
    #[must_use]
    pub fn first(&self, reading: Monotonic) -> Option<Interval> {
        let first = self.cell.read(decode_first)?;
        Some(Slew::new(first).at(reading, DRIFT).interval())
    }
}

// A push that slews changes only words 1 to 5, and an update stores only the words
// that change.
fn encode(discipline: Discipline, first: Option<Measurement>) -> [u64; WORDS] {
    let count = |n: usize| u64::try_from(n).expect("invariant: a count fits in u64");
    let (kind, slew, failure) = match discipline {
        Discipline::Unsynced(failure) => (0, None, Some(failure)),
        Discipline::Synced(slew) => (1, Some(slew), None),
        Discipline::Holdover(slew, Cause::NoEstimate(failure)) => {
            (2, Some(slew), Some(failure))
        }
        Discipline::Holdover(slew, Cause::UnknownEstimate) => (3, Some(slew), None),
    };
    let [start, from, at, offset, error] = slew.map_or([0; 5], encode_slew);
    let [first_at, first_offset, first_error] =
        first.map_or([0; 3], encode_measurement);
    let [sources, agreeing, empty] = match failure {
        Some(combine::Error::NoMajority {
            sources,
            agreeing,
            empty,
        }) => [count(sources), count(agreeing), count(empty)],
        Some(combine::Error::NoSources) | None => [0; 3],
    };
    [
        kind,
        start,
        from,
        at,
        offset,
        error,
        sources,
        agreeing,
        empty,
        first_at,
        first_offset,
        first_error,
    ]
}

fn decode(words: [u64; WORDS]) -> Discipline {
    let [
        kind,
        start,
        from,
        at,
        offset,
        error,
        sources,
        agreeing,
        empty,
        ..,
    ] = words;
    let slew = || decode_slew([start, from, at, offset, error]);
    let count =
        |word: u64| usize::try_from(word).expect("invariant: a count from a usize");
    // `NoMajority` has at least 1 source.
    let failure = || match sources {
        0 => combine::Error::NoSources,
        _ => combine::Error::NoMajority {
            sources: count(sources),
            agreeing: count(agreeing),
            empty: count(empty),
        },
    };
    match kind {
        0 => Discipline::Unsynced(failure()),
        1 => Discipline::Synced(slew()),
        2 => Discipline::Holdover(slew(), Cause::NoEstimate(failure())),
        3 => Discipline::Holdover(slew(), Cause::UnknownEstimate),
        _ => panic!("invariant: the cell holds discipline {kind}"),
    }
}

/// The first estimate, which the cell holds from the first slew on.
fn decode_first(words: [u64; WORDS]) -> Option<Measurement> {
    let [.., at, offset, error] = words;
    decode(words)
        .slew()
        .map(|_| decode_measurement([at, offset, error]))
}

fn encode_slew(slew: Slew) -> [u64; 5] {
    let [at, offset, error] = encode_measurement(slew.target);
    [
        slew.start.0,
        slew.from.nanos().cast_unsigned(),
        at,
        offset,
        error,
    ]
}

fn decode_slew(words: [u64; 5]) -> Slew {
    let [start, from, at, offset, error] = words;
    Slew {
        start: Monotonic(start),
        from: Span::from_nanos(from.cast_signed()),
        target: decode_measurement([at, offset, error]),
    }
}

fn encode_measurement(m: Measurement) -> [u64; 3] {
    let span = |span: Span| span.nanos().cast_unsigned();
    [m.at().0, span(m.offset()), span(m.error())]
}

fn decode_measurement(words: [u64; 3]) -> Measurement {
    let [at, offset, error] = words;
    let span = |word: u64| Span::from_nanos(word.cast_signed());
    let Some(m) = Measurement::new(Monotonic(at), span(offset), span(error)) else {
        panic!("invariant: the cell holds a measurement");
    };
    m
}

#[cfg(test)]
mod tests {
    use estimate::combine::Error;
    use estimate::discipline::{Cause, Discipline};
    use estimate::{Measurement, Slew};
    use proptest::prelude::*;
    use types::time::{Monotonic, Span};

    use super::{decode, decode_first, encode};

    /// The largest error a measurement has.
    const UNKNOWN: Span = Measurement::unknown(Monotonic(0), Span::ZERO).error();

    fn slew() -> impl Strategy<Value = Slew> {
        let error = 0..=UNKNOWN.nanos();
        let words = (
            any::<u64>(),
            any::<i64>(),
            any::<u64>(),
            any::<i64>(),
            error,
        );
        words.prop_map(|(start, from, at, offset, error)| {
            let target = Measurement::new(
                Monotonic(at),
                Span::from_nanos(offset),
                Span::from_nanos(error),
            );
            Slew {
                start: Monotonic(start),
                from: Span::from_nanos(from),
                target: target.expect("at most 36500 days"),
            }
        })
    }

    fn error() -> impl Strategy<Value = Error> {
        let counts = (1..=usize::MAX, any::<usize>(), any::<usize>());
        prop_oneof![
            Just(Error::NoSources),
            counts.prop_map(|(sources, agreeing, empty)| Error::NoMajority {
                sources,
                agreeing,
                empty,
            }),
        ]
    }

    fn discipline() -> impl Strategy<Value = Discipline> {
        let cause = prop_oneof![
            error().prop_map(Cause::NoEstimate),
            Just(Cause::UnknownEstimate)
        ];
        prop_oneof![
            error().prop_map(Discipline::Unsynced),
            slew().prop_map(Discipline::Synced),
            (slew(), cause).prop_map(|(slew, cause)| Discipline::Holdover(slew, cause)),
        ]
    }

    proptest! {
        #[test]
        fn a_discipline_and_the_first_estimate_round_trip_through_the_cell(
            discipline in discipline(),
            other in slew(),
        ) {
            let first = discipline.slew().map(|_| other.target);
            let words = encode(discipline, first);
            prop_assert_eq!(decode(words), discipline);
            prop_assert_eq!(decode_first(words), first);
        }
    }
}
