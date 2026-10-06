//! The two cells that carry a clock's discipline to its readers: the discipline, and
//! the first slew.

use estimate::combine;
use estimate::discipline::{Cause, Discipline};
use estimate::{Measurement, Slew};
use types::time::{Monotonic, Span};

/// The words of the cell that holds a [`Discipline`]: its kind, a [`Slew`], and a
/// [`combine::Error`].
const WORDS: usize = 9;

/// The words of the cell that holds the first slew: 1 when it is there, then the
/// [`Slew`].
const FIRST_WORDS: usize = 6;

/// Makes the cells, with no sources and no first slew.
pub(crate) fn new() -> (Writer, Reader) {
    let discipline = Discipline::Unsynced(combine::Error::NoSources);
    let (cell, discipline_reader) = ring::latest::new(encode(discipline));
    let (first, first_reader) = ring::latest::new(encode_first(None));
    let writer = Writer {
        discipline,
        cell,
        first: Some(first),
    };
    let reader = Reader {
        discipline: discipline_reader,
        first: first_reader,
    };
    (writer, reader)
}

/// The one writer of the cells.
#[derive(Debug)]
pub(crate) struct Writer {
    discipline: Discipline,
    cell: ring::latest::Writer<WORDS>,
    /// `None` once it holds the first slew.
    first: Option<ring::latest::Writer<FIRST_WORDS>>,
}

impl Writer {
    /// The discipline the last update stored.
    pub(crate) fn discipline(&self) -> Discipline {
        self.discipline
    }

    /// Stores the discipline that `next` gives. `next` runs inside the update, as `f`
    /// does in [`ring::latest::Writer::update`]. The slew of the first discipline with
    /// one goes into the first cell inside the same update, so a read of a discipline
    /// with that slew comes after the write of the first cell.
    pub(crate) fn update(&mut self, next: impl FnOnce() -> Discipline) {
        let Self {
            discipline,
            cell,
            first,
        } = self;
        cell.update(|_| {
            *discipline = next();
            if let Some(cell) = first.as_mut()
                && let Some(slew) = discipline.slew()
            {
                cell.update(|_| encode_first(Some(slew)));
                *first = None;
            }
            encode(*discipline)
        });
    }
}

/// Reads the cells from any thread with no lock.
#[derive(Clone, Debug)]
pub(crate) struct Reader {
    discipline: ring::latest::Reader<WORDS>,
    first: ring::latest::Reader<FIRST_WORDS>,
}

impl Reader {
    /// Runs `f` on the newest discipline, as [`ring::latest::Reader::read`] runs it
    /// on the newest value.
    pub(crate) fn read<R>(&self, mut f: impl FnMut(Discipline) -> R) -> R {
        self.discipline.read(|words| f(decode(words)))
    }

    /// The first slew, or `None` before the first discipline with one.
    pub(crate) fn first(&self) -> Option<Slew> {
        self.first.read(decode_first)
    }
}

// A push that slews changes only words 1 to 5, and an update stores only the words
// that change.
fn encode(discipline: Discipline) -> [u64; WORDS] {
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
    let [sources, agreeing, empty] = match failure {
        Some(combine::Error::NoMajority {
            sources,
            agreeing,
            empty,
        }) => [count(sources), count(agreeing), count(empty)],
        Some(combine::Error::NoSources) | None => [0; 3],
    };
    [
        kind, start, from, at, offset, error, sources, agreeing, empty,
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

fn encode_first(first: Option<Slew>) -> [u64; FIRST_WORDS] {
    let [start, from, at, offset, error] = first.map_or([0; 5], encode_slew);
    [u64::from(first.is_some()), start, from, at, offset, error]
}

fn decode_first(words: [u64; FIRST_WORDS]) -> Option<Slew> {
    let [present, slew @ ..] = words;
    match present {
        0 => None,
        1 => Some(decode_slew(slew)),
        _ => panic!("invariant: the first cell holds presence {present}"),
    }
}

fn encode_slew(slew: Slew) -> [u64; 5] {
    let span = |span: Span| span.nanos().cast_unsigned();
    let target = slew.target;
    [
        slew.start.0,
        span(slew.from),
        target.at().0,
        span(target.offset()),
        span(target.error()),
    ]
}

fn decode_slew(words: [u64; 5]) -> Slew {
    let [start, from, at, offset, error] = words;
    let span = |word: u64| Span::from_nanos(word.cast_signed());
    let Some(target) = Measurement::new(Monotonic(at), span(offset), span(error))
    else {
        panic!("invariant: the cell holds a measurement");
    };
    Slew {
        start: Monotonic(start),
        from: span(from),
        target,
    }
}

#[cfg(test)]
mod tests {
    use estimate::combine::Error;
    use estimate::discipline::{Cause, Discipline};
    use estimate::{Measurement, Slew};
    use proptest::prelude::*;
    use types::time::{Monotonic, Span};

    use super::{decode, decode_first, encode, encode_first, new};

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
        fn a_discipline_round_trips_through_the_cell(discipline in discipline()) {
            prop_assert_eq!(decode(encode(discipline)), discipline);
        }

        #[test]
        fn the_first_slew_round_trips_through_its_cell(
            first in prop::option::of(slew()),
        ) {
            prop_assert_eq!(decode_first(encode_first(first)), first);
        }

        #[test]
        fn keeps_the_last_discipline_and_the_first_slew(
            disciplines in prop::collection::vec(discipline(), 0..8),
        ) {
            let (mut writer, reader) = new();
            for &discipline in &disciplines {
                writer.update(|| discipline);
            }
            let last = disciplines.last().copied();
            let last = last.unwrap_or(Discipline::Unsynced(Error::NoSources));
            prop_assert_eq!(writer.discipline(), last);
            prop_assert_eq!(reader.read(|discipline| discipline), last);
            let first = disciplines.iter().find_map(|discipline| discipline.slew());
            prop_assert_eq!(reader.first(), first);
        }
    }
}
