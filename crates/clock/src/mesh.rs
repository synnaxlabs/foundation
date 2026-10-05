use estimate::combine::{self, combine};
use estimate::{Filter, Measurement, Slew};
use types::hash::Map;
use types::time::{Interval, Monotonic, Span};

use crate::{DRIFT, source};

/// The words of the cell: a [`State`] as its kind, a [`Slew`], and a
/// [`combine::Error`].
const WORDS: usize = 9;

/// The mesh clock of one node. It lives on one shard, and [`Reader`]s read it from
/// any.
#[derive(Debug)]
pub struct Clock {
    monotonic: env::clock::Clock,
    sources: Map<source::Key, Filter>,
    next: u64,
    state: State,
    cell: ring::latest::Writer<WORDS>,
}

impl Clock {
    /// Makes a clock with no sources, and a reader for it. Mesh time runs on
    /// `monotonic`, the node's monotonic clock. Readers have no time until a majority
    /// of the sources first agree.
    #[must_use]
    pub fn new(monotonic: env::clock::Clock) -> (Self, Reader) {
        let (cell, reader) = ring::latest::new([0; WORDS]);
        let reader = Reader {
            monotonic: monotonic.clone(),
            cell: reader,
        };
        let clock = Self {
            monotonic,
            sources: Map::default(),
            next: 0,
            state: State::Unsynced(combine::Error::NoSources),
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
    pub fn remove(&mut self, source: source::Key) -> Status {
        let removed = self.sources.remove(&source);
        assert!(removed.is_some(), "{source:?} was removed");
        self.steer()
    }

    /// Records a measurement of the monotonic clock from `source`, then moves mesh
    /// time toward the estimate of all sources.
    ///
    /// # Panics
    ///
    /// When `source` was removed.
    pub fn push(&mut self, source: source::Key, measurement: Measurement) -> Status {
        let Some(filter) = self.sources.get_mut(&source) else {
            panic!("{source:?} was removed");
        };
        filter.push(measurement);
        self.steer()
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

    fn steer(&mut self) -> Status {
        let estimate = combine(self.monotonic.now(), DRIFT, self.sources.values());
        let mut state = self.state;
        self.cell.update(|_| {
            state = match (state.slew(), estimate) {
                (None, Err(e)) => State::Unsynced(e),
                (Some(slew), Err(e)) => State::Holdover(slew, e),
                // Before the first estimate, readers have no time that could go back.
                (None, Ok(estimate)) => State::Synced(Slew::new(estimate)),
                // `toward` keeps mesh time from going back only against reads before
                // its `now`, so the clock reads inside the update.
                (Some(old), Ok(estimate)) => {
                    State::Synced(old.toward(self.monotonic.now(), DRIFT, estimate))
                }
            };
            encode(state)
        });
        self.state = state;
        state.at(self.monotonic.now())
    }
}

/// A [`Status`] with the slew in place of mesh time at one reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Unsynced(combine::Error),
    Synced(Slew),
    Holdover(Slew, combine::Error),
}

impl State {
    fn slew(self) -> Option<Slew> {
        match self {
            Self::Unsynced(_) => None,
            Self::Synced(slew) | Self::Holdover(slew, _) => Some(slew),
        }
    }

    fn at(self, now: Monotonic) -> Status {
        match self {
            Self::Unsynced(e) => Status::Unsynced(e),
            Self::Synced(slew) => Status::Synced(slew.at(now, DRIFT)),
            Self::Holdover(slew, e) => Status::Holdover(slew.at(now, DRIFT), e),
        }
    }
}

/// What a clock follows after a change to its sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No majority of the sources has agreed yet, so readers have no time. A source
    /// with no measurement counts against a majority. Holds why.
    Unsynced(combine::Error),
    /// A majority of the sources agrees. Holds mesh time now, as an offset from the
    /// monotonic clock with its error. The error can be unknown
    /// ([`Measurement::unknown`]).
    Synced(Measurement),
    /// No majority agrees now, or no source is left. Mesh time keeps its slew, and its
    /// error grows by drift. Holds mesh time now and why.
    Holdover(Measurement, combine::Error),
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

    /// What the clock follows now: the status after its last change of sources, with
    /// mesh time at the call. [`Status::Unsynced`] until a majority of the sources
    /// first agree.
    #[must_use]
    pub fn status(&self) -> Status {
        self.cell
            .read(|words| decode(words).at(self.monotonic.now()))
    }
}

fn encode(state: State) -> [u64; WORDS] {
    let span = |span: Span| span.nanos().cast_unsigned();
    let count = |n: usize| u64::try_from(n).expect("invariant: a count fits in u64");
    let (kind, slew, cause) = match state {
        State::Unsynced(cause) => (0, None, Some(cause)),
        State::Synced(slew) => (1, Some(slew), None),
        State::Holdover(slew, cause) => (2, Some(slew), Some(cause)),
    };
    let mut words = [kind, 0, 0, 0, 0, 0, 0, 0, 0];
    if let Some(slew) = slew {
        let target = slew.target;
        words[1..6].copy_from_slice(&[
            slew.start.0,
            span(slew.from),
            target.at().0,
            span(target.offset()),
            span(target.error()),
        ]);
    }
    // `NoSources` is 0 sources: `NoMajority` has at least 1.
    if let Some(combine::Error::NoMajority {
        sources,
        agreeing,
        empty,
    }) = cause
    {
        words[6..].copy_from_slice(&[count(sources), count(agreeing), count(empty)]);
    }
    words
}

fn decode(words: [u64; WORDS]) -> State {
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
    ] = words;
    let span = |word: u64| Span::from_nanos(word.cast_signed());
    let count =
        |word: u64| usize::try_from(word).expect("invariant: a count from a usize");
    let slew = || {
        let Some(target) = Measurement::new(Monotonic(at), span(offset), span(error))
        else {
            panic!("invariant: the cell holds a measurement");
        };
        Slew {
            start: Monotonic(start),
            from: span(from),
            target,
        }
    };
    let cause = || match sources {
        0 => combine::Error::NoSources,
        _ => combine::Error::NoMajority {
            sources: count(sources),
            agreeing: count(agreeing),
            empty: count(empty),
        },
    };
    match kind {
        0 => State::Unsynced(cause()),
        1 => State::Synced(slew()),
        2 => State::Holdover(slew(), cause()),
        _ => panic!("invariant: the cell holds state {kind}"),
    }
}

#[cfg(test)]
mod tests {
    use estimate::combine::Error;
    use estimate::{Measurement, Slew};
    use proptest::prelude::*;
    use types::time::{Monotonic, Span};

    use super::{State, decode, encode};

    fn slew() -> impl Strategy<Value = Slew> {
        let error = 0..=36_500 * Span::DAY.nanos();
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

    fn cause() -> impl Strategy<Value = Error> {
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

    fn state() -> impl Strategy<Value = State> {
        prop_oneof![
            cause().prop_map(State::Unsynced),
            slew().prop_map(State::Synced),
            (slew(), cause()).prop_map(|(slew, cause)| State::Holdover(slew, cause)),
        ]
    }

    proptest! {
        #[test]
        fn a_state_round_trips_through_the_cell(state in state()) {
            prop_assert_eq!(decode(encode(state)), state);
        }
    }

    #[test]
    fn an_empty_cell_is_unsynced_with_no_sources() {
        assert_eq!(decode([0; 9]), State::Unsynced(Error::NoSources));
    }
}
