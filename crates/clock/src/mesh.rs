use estimate::combine::{self, combine};
use estimate::{Filter, Measurement, Slew};
use types::hash::Map;
use types::time::{Interval, Monotonic, Span};

use crate::{DRIFT, source};

/// The words of the cell: 1 once readers have time, then a [`Slew`].
const WORDS: usize = 6;

/// The mesh clock of one node. It lives on one shard, and [`Reader`]s read it from
/// any.
#[derive(Debug)]
pub struct Clock {
    monotonic: env::clock::Clock,
    sources: Map<source::Key, Filter>,
    next: u64,
    slew: Option<Slew>,
    cell: ring::latest::Writer<WORDS>,
}

impl Clock {
    /// Makes a clock with no sources, and a reader for it. Mesh time runs on
    /// `monotonic`, the node's monotonic clock. Readers have no time until the first
    /// measurement.
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
            slew: None,
            cell,
        };
        (clock, reader)
    }

    /// Adds a source with no measurements and returns its key.
    pub fn add(&mut self) -> source::Key {
        let key = source::Key(self.next);
        self.next += 1;
        self.sources.insert(key, Filter::default());
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

    fn steer(&mut self) -> Status {
        let now = self.monotonic.now();
        let estimate = combine(now, DRIFT, self.sources.values());
        let (old, estimate) = match (self.slew, estimate) {
            (None, Err(_)) => return Status::Unsynced,
            (Some(slew), Err(e)) => return Status::Holdover(slew.at(now, DRIFT), e),
            (old, Ok(estimate)) => (old, estimate),
        };
        let mut steered = (Slew::new(estimate), now);
        self.cell.update(|_| {
            // `toward` keeps mesh time from going back only against reads before its
            // `now`, so the clock reads inside the update.
            if let Some(old) = old {
                let now = self.monotonic.now();
                steered = (old.toward(now, DRIFT, estimate), now);
            }
            encode(steered.0)
        });
        let (slew, now) = steered;
        self.slew = Some(slew);
        Status::Synced(slew.at(now, DRIFT))
    }
}

/// What a clock follows after a change to its sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No source has a measurement yet, so readers have no time.
    Unsynced,
    /// A majority of the sources with measurements agrees. Holds mesh time now, as an
    /// offset from the monotonic clock with its error. An error of 36500 days is
    /// unknown.
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
    /// an unknown error). `None` until the first measurement.
    #[must_use]
    pub fn now(&self) -> Option<Interval> {
        self.cell.read(|words| {
            let slew = decode(words)?;
            Some(slew.at(self.monotonic.now(), DRIFT).interval())
        })
    }
}

fn encode(slew: Slew) -> [u64; WORDS] {
    let span = |span: Span| span.nanos().cast_unsigned();
    let target = slew.target;
    [
        1,
        slew.start.0,
        span(slew.from),
        target.at().0,
        span(target.offset()),
        span(target.error()),
    ]
}

fn decode(words: [u64; WORDS]) -> Option<Slew> {
    let [time, start, from, at, offset, error] = words;
    if time == 0 {
        return None;
    }
    let span = |word: u64| Span::from_nanos(word.cast_signed());
    let Some(target) = Measurement::new(Monotonic(at), span(offset), span(error))
    else {
        panic!("invariant: the cell holds a measurement");
    };
    Some(Slew {
        start: Monotonic(start),
        from: span(from),
        target,
    })
}

#[cfg(test)]
mod tests {
    use estimate::{Measurement, Slew};
    use proptest::prelude::*;
    use types::time::{Monotonic, Span};

    use super::{decode, encode};

    proptest! {
        #[test]
        fn a_slew_round_trips_through_the_cell(
            start in any::<u64>(),
            from in any::<i64>(),
            at in any::<u64>(),
            offset in any::<i64>(),
            error in 0..=36_500 * Span::DAY.nanos(),
        ) {
            let target = Measurement::new(
                Monotonic(at),
                Span::from_nanos(offset),
                Span::from_nanos(error),
            );
            let slew = Slew {
                start: Monotonic(start),
                from: Span::from_nanos(from),
                target: target.expect("at most 36500 days"),
            };
            prop_assert_eq!(decode(encode(slew)), Some(slew));
        }
    }

    #[test]
    fn an_empty_cell_has_no_time() {
        assert_eq!(decode([0; 6]), None);
    }
}
