//! `wire::hub::write::Frames`, driven as a caller drives it by `Frames::due`, never
//! panics. Each head that it gives encodes back to its message, so it gives no head
//! that `Head::encode` panics on. Each error is one that `decode` lists for the part
//! that was due, and each body is as long as its layout says. Each frame has the time
//! and form of its head, a range for each group whose index it holds, in group order,
//! and the counts of its run of counts, or 1 for each group of a head that the node
//! stamps. A frame that finds the pool full is skipped, and the stream goes on. At the
//! end of the input, `finish` gives `Ok` between frames, else `Error::Cut` with the
//! bytes of the frame that came. A run that receives each body message in place gives
//! the same steps as a run that gives it whole to `decode`.
//!
//! Input: one byte, whose low 3 bits are the places of the stream less 1, and whose
//! bit 3 splits a stream of at least 2 places into two groups, then the messages from
//! the program (`fuzz::messages`).

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{Form, Path, Range};
use types::sample::{Scalar, Type};
use wire::hub::Error;
use wire::hub::write::{Due, Frames, Head, Step, Time};

/// The bytes of one count of a run of counts.
const COUNT: usize = 4;

/// The bytes of one end of a run of ends.
const END: usize = 8;

/// What one step gave, with a frame's series copied out.
#[derive(Debug, PartialEq)]
enum Seen {
    Head(Head),
    More,
    /// A message of the run of counts, with its counts.
    Counts(Vec<u32>),
    Frame {
        time: Time,
        form: Form,
        ranges: Vec<(u32, Range)>,
        series: Vec<Option<Vec<u8>>>,
    },
    /// The error of `decode` or `received`, which stops the stream.
    Message(Error),
    /// The error of a draft's layout, which stops the stream.
    Layout(types::frame::Error),
    /// The pool had no block for the draft, so the frame was skipped.
    Skipped,
    /// The input ended, with what `finish` gave.
    Finished(Result<(), Error>),
}

/// An index, then `places - 1` channels of `I64`, in place order. With `two` and at
/// least 2 places, the second half of the places is a second group, whose index is
/// its first place.
fn set(places: u8, two: bool) -> Arc<KeySet> {
    let split = if two && places > 1 {
        usize::from(places.div_ceil(2))
    } else {
        usize::MAX
    };
    let channels = (0..usize::from(places)).map(|entry| {
        let index = if entry >= split { split } else { 0 };
        let data = if entry == index {
            Scalar::Stamp
        } else {
            Scalar::I64
        };
        let key = u128::try_from(entry).expect("at most 8 places") + 1;
        (channel::Key::from_u128(key), Type::Scalar(data), index)
    });
    Arc::new(KeySet::ordered(channels).expect("a valid set"))
}

fn pool() -> block::Pool {
    let config = block::Config::new(1 << 16).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

impl Seen {
    fn of(step: Step, places: u8) -> Self {
        match step {
            Step::Head(head) => Self::Head(head),
            Step::More => Self::More,
            Step::Frame { draft, time } => {
                let form = draft.form();
                let ranges = draft.ranges().collect();
                let frame = draft.freeze(Path::Live);
                Self::Frame {
                    time,
                    form,
                    ranges,
                    series: (0..usize::from(places))
                        .map(|entry| frame.series(entry).map(<[u8]>::to_vec))
                        .collect(),
                }
            }
        }
    }
}

/// The part of a frame that the stream expects, as the steps show it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Head,
    Ends,
    /// The run of counts, with the counts that remain, then the bytes of the body.
    /// Skipped when the pool had no block.
    Counts {
        remain: usize,
        body: usize,
        skipped: bool,
    },
    /// The body, with its bytes that remain.
    Body {
        remain: usize,
        skipped: bool,
    },
}

/// Checks that `error` is one that `decode` lists for `part`.
fn check_error(error: Error, part: Part) {
    let listed = match part {
        Part::Head => matches!(
            error,
            Error::Empty
                | Error::Kind { .. }
                | Error::Length { .. }
                | Error::Path { .. }
                | Error::Form { .. }
                | Error::Series
                | Error::Stamp
                | Error::Places { .. }
        ),
        Part::Ends => matches!(
            error,
            Error::Empty | Error::Length { .. } | Error::Run { .. }
        ),
        Part::Counts { .. } => matches!(
            error,
            Error::Empty | Error::Length { .. } | Error::Run { .. } | Error::Samples
        ),
        Part::Body { .. } => matches!(error, Error::Empty | Error::Body { .. }),
    };
    assert!(listed, "{part:?} gave {error:?}");
}

/// The part after `step`, which took `len` bytes in `part`.
fn after(step: &Step, part: Part, len: usize) -> Part {
    match (step, part) {
        (Step::Head(_), Part::Head) => Part::Ends,
        (Step::More, Part::Ends) => Part::Ends,
        (
            step,
            Part::Counts {
                remain,
                body,
                skipped,
            },
        ) => {
            let remain = remain
                .checked_sub(len / COUNT)
                .expect("a message of counts fits its run");
            let next = counted(remain, body, skipped);
            check_frame(step, next, skipped);
            next
        }
        (Step::More, Part::Body { remain, skipped }) => {
            let remain = remain
                .checked_sub(len)
                .expect("a body message fits its body");
            match (remain, skipped) {
                (0, true) => Part::Head,
                (0, false) => panic!("a whole body gave no frame"),
                _ => Part::Body { remain, skipped },
            }
        }
        (
            Step::Frame { .. },
            Part::Body {
                remain,
                skipped: false,
            },
        ) => {
            assert_eq!(remain, len, "a frame came before the end of its body");
            Part::Head
        }
        (step, part) => panic!("{part:?} gave {step:?}"),
    }
}

/// The part after a draft or a skip, or a message of the run of counts, with the
/// `remain` counts of the run and the `body` bytes that are due.
fn counted(remain: usize, body: usize, skipped: bool) -> Part {
    match (remain, body) {
        (0, 0) => Part::Head,
        (0, remain) => Part::Body { remain, skipped },
        _ => Part::Counts {
            remain,
            body,
            skipped,
        },
    }
}

/// Checks that `step` gives a frame when its frame ends at `next` and was not
/// `skipped`, and no frame else.
fn check_frame(step: &Step, next: Part, skipped: bool) {
    assert_eq!(
        matches!(step, Step::Frame { .. }),
        next == Part::Head && !skipped,
        "{next:?} came with {step:?}"
    );
}

/// The counts of the run of a frame of `time` whose ends hold `entries` of `set`:
/// one for each index, or none for a head that the node stamps.
fn counts(set: &KeySet, time: Time, entries: &[usize]) -> usize {
    match time {
        Time::Frame(_) => entries
            .iter()
            .filter(|entry| set.groups().contains(entry))
            .count(),
        Time::Node => 0,
    }
}

/// The items of `message`, `N` bytes each, with each first `u32`.
fn items<const N: usize>(message: &[u8]) -> impl Iterator<Item = u32> + '_ {
    message.chunks_exact(N).map(|item| {
        let first = item.first_chunk().expect("an item holds a u32");
        u32::from_le_bytes(*first)
    })
}

/// The steps of a stream of `set` that reads `bytes`, each body message received in
/// place when `in_place` and it fits the body.
fn run(set: &Arc<KeySet>, bytes: &[u8], in_place: bool) -> Vec<Seen> {
    let places = u8::try_from(set.entries().len()).expect("at most 8 places");
    let pool = pool();
    let mut frames = Frames::new(Arc::clone(set));
    let mut messages = fuzz::messages(bytes);
    let mut seen = Vec::new();
    let mut part = Part::Head;
    let mut time = Time::Node;
    // The entries of the ends of the frame that came.
    let mut entries = Vec::new();
    // The bytes of the frame that came.
    let mut received = 0;
    loop {
        let len;
        // The counts of a message of the run of counts.
        let mut given = None;
        let step = match frames.due() {
            Err(error) => {
                assert_eq!(
                    frames.due().map(drop),
                    Err(error.clone()),
                    "the layout changed"
                );
                seen.push(Seen::Layout(error));
                return seen;
            }
            Ok(Due::Draft { layout, form }) => {
                assert_eq!(part, Part::Ends, "a draft is due outside the ends");
                let body = layout.body_len();
                let remain = counts(set, time, &entries);
                match layout.draft(&pool, form) {
                    Ok(draft) => {
                        part = counted(remain, body, false);
                        let step = frames.draft(draft);
                        check_frame(&step, part, false);
                        seen.push(Seen::of(step, places));
                    }
                    Err(_) => {
                        part = counted(remain, body, true);
                        frames.skip();
                        seen.push(Seen::Skipped);
                    }
                }
                if part == Part::Head {
                    received = 0;
                }
                continue;
            }
            Ok(Due::Body(rest)) => {
                let Part::Body {
                    remain,
                    skipped: false,
                } = part
                else {
                    panic!("{part:?} gave a body slice");
                };
                assert_eq!(rest.len(), remain, "the slice is not the rest of the body");
                let Some(message) = messages.next() else {
                    seen.push(finished(&frames, received));
                    return seen;
                };
                len = message.len();
                let step = match rest.get_mut(..message.len()) {
                    Some(into) if in_place => {
                        into.copy_from_slice(message);
                        frames.received(message.len())
                    }
                    _ => frames.decode(message),
                };
                match step {
                    Ok(step) => step,
                    Err(error) => return stopped(seen, error, part),
                }
            }
            Ok(Due::Message) => {
                assert!(
                    !matches!(part, Part::Body { skipped: false, .. }),
                    "{part:?} gave a whole message"
                );
                let Some(message) = messages.next() else {
                    seen.push(finished(&frames, received));
                    return seen;
                };
                len = message.len();
                match frames.decode(message) {
                    Ok(Step::Head(head)) => {
                        let mut out = [0; Head::LEN];
                        head.encode(&mut out);
                        assert_eq!(&out[..], message, "the head changed");
                        time = head.time;
                        entries.clear();
                        Step::Head(head)
                    }
                    Ok(step) => {
                        match part {
                            Part::Ends => {
                                entries.extend(items::<END>(message).map(|entry| {
                                    usize::try_from(entry).expect("a u32 fits")
                                }))
                            }
                            Part::Counts { .. } => {
                                given = Some(items::<COUNT>(message).collect());
                            }
                            _ => {}
                        }
                        step
                    }
                    Err(error) => return stopped(seen, error, part),
                }
            }
        };
        part = after(&step, part, len);
        received = if part == Part::Head {
            0
        } else {
            received + len
        };
        if let Some(counts) = given {
            seen.push(Seen::Counts(counts));
        }
        seen.push(Seen::of(step, places));
    }
}

/// `seen` with the `error` that stopped the stream in `part`.
fn stopped(mut seen: Vec<Seen>, error: Error, part: Part) -> Vec<Seen> {
    check_error(error, part);
    seen.push(Seen::Message(error));
    seen
}

/// What `finish` gave at the end of the input, checked against the `received` bytes
/// of the frame that came.
fn finished(frames: &Frames, received: usize) -> Seen {
    let finish = frames.finish();
    match finish {
        Ok(()) => assert_eq!(received, 0, "a cut frame finished"),
        Err(Error::Cut { received: cut, len }) => {
            assert_eq!(cut, received, "the cut has other bytes");
            assert!(
                len.is_none_or(|len| len.get() > cut),
                "the cut has its bytes"
            );
        }
        Err(error) => panic!("finish gave {error:?}"),
    }
    Seen::Finished(finish)
}

/// Checks that each frame of `seen` has the time and form of the head before it, a
/// range for each group of `set` whose index it holds, in group order, and the counts
/// of its run of counts, or 1 for each group of a head that the node stamps.
fn check(set: &KeySet, seen: &[Seen]) {
    let (mut head, mut counts) = (None, Vec::new());
    for step in seen {
        match step {
            Seen::Head(next) => {
                head = Some(*next);
                counts.clear();
            }
            Seen::Counts(given) => counts.extend_from_slice(given),
            Seen::Skipped => {
                head.take().expect("a skipped frame came with no head");
            }
            Seen::Frame {
                time,
                form,
                ranges,
                series,
            } => {
                let head = head.take().expect("a frame came with no head");
                assert_eq!(*time, head.time, "the frame changed its time");
                assert_eq!(*form, head.form, "the frame changed its form");
                let groups: Vec<u32> = set
                    .groups()
                    .iter()
                    .zip(0..)
                    .filter(|&(&index, _)| {
                        series.get(index).is_some_and(Option::is_some)
                    })
                    .map(|(_, group)| group)
                    .collect();
                let counts = match head.time {
                    Time::Frame(_) => counts.clone(),
                    Time::Node => vec![1; groups.len()],
                };
                let want: Vec<(u32, Range)> = groups
                    .into_iter()
                    .zip(counts)
                    .map(|(group, count)| (group, Range { seq: 0, count }))
                    .collect();
                assert_eq!(*ranges, want, "the frame changed its ranges");
            }
            _ => {}
        }
    }
}

fuzz_target!(|bytes: &[u8]| {
    let Some((&first, bytes)) = bytes.split_first() else {
        return;
    };
    let set = set((first & 0b111) + 1, first & 0b1000 != 0);
    let whole = run(&set, bytes, false);
    check(&set, &whole);
    assert_eq!(run(&set, bytes, true), whole, "the two runs differ");
});
