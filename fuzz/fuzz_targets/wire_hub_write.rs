//! `wire::hub::write::Frames`, driven as a caller drives it by `Frames::due`, never
//! panics. Each head that it gives encodes back to its message, so it gives no head
//! that `Head::encode` panics on. Each error is one that `decode` lists for the part
//! that was due, and each body is as long as its layout says. Each frame has the time,
//! form, and count of its head. A frame that finds the pool full is skipped, and the
//! stream goes on. At the end of the input, `finish` gives `Ok` between frames, else
//! `Error::Cut` with the bytes of the frame that came. A run that receives each body
//! message in place gives the same steps as a run that gives it whole to `decode`.
//!
//! Input: one byte, whose low 3 bits are the places of the stream less 1, then the
//! messages from the program (`fuzz::messages`).

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

/// What one step gave, with a frame's series copied out.
#[derive(Debug, PartialEq)]
enum Seen {
    Head(Head),
    More,
    Frame {
        time: Time,
        form: Form,
        range: Option<Range>,
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

/// An index, then `places - 1` channels of `I64`, in place order.
fn set(places: u8) -> Arc<KeySet> {
    let channels = (1..=places).map(|n| {
        let data = if n == 1 { Scalar::Stamp } else { Scalar::I64 };
        (
            channel::Key::from_u128(u128::from(n)),
            Type::Scalar(data),
            0,
        )
    });
    Arc::new(KeySet::ordered(channels).expect("no key repeats"))
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
                let range = draft.range(0);
                let frame = draft.freeze(Path::Live);
                Self::Frame {
                    time,
                    form,
                    range,
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
    /// The body, with its bytes that remain, skipped when the pool had no block.
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
                | Error::Samples
                | Error::Series
                | Error::Stamp { .. }
                | Error::Places { .. }
        ),
        Part::Ends => matches!(
            error,
            Error::Empty | Error::Length { .. } | Error::Run { .. }
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

/// The steps of a stream of `places` that reads `bytes`, each body message received
/// in place when `in_place` and it fits the body.
fn run(places: u8, bytes: &[u8], in_place: bool) -> Vec<Seen> {
    let pool = pool();
    let mut frames = Frames::new(set(places));
    let mut messages = fuzz::messages(bytes);
    let mut seen = Vec::new();
    let mut part = Part::Head;
    // The bytes of the frame that came.
    let mut received = 0;
    loop {
        let mut len = 0;
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
                let remain = layout.body_len();
                part = Part::Body {
                    remain,
                    skipped: false,
                };
                match layout.draft(&pool, form) {
                    Ok(draft) => frames.draft(draft),
                    Err(_) => {
                        frames.skip();
                        seen.push(Seen::Skipped);
                        part = match remain {
                            0 => Part::Head,
                            _ => Part::Body {
                                remain,
                                skipped: true,
                            },
                        };
                        if part == Part::Head {
                            received = 0;
                        }
                        continue;
                    }
                }
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
                    Ok(Step::Head(given)) => {
                        let mut out = [0; Head::LEN];
                        given.encode(&mut out);
                        assert_eq!(&out[..], message, "the head changed");
                        Step::Head(given)
                    }
                    Ok(step) => step,
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

/// Checks that each frame of `seen` has the time, form, and count of the head before
/// it.
fn check(seen: &[Seen]) {
    let mut head = None;
    for step in seen {
        match step {
            Seen::Head(next) => head = Some(*next),
            Seen::Skipped => {
                head.take().expect("a skipped frame came with no head");
            }
            Seen::Frame {
                time, form, range, ..
            } => {
                let head = head.take().expect("a frame came with no head");
                assert_eq!(*time, head.time, "the frame changed its time");
                assert_eq!(*form, head.form, "the frame changed its form");
                let count = Some(Range {
                    seq: 0,
                    count: head.count,
                });
                assert_eq!(*range, count, "the frame changed its count");
            }
            _ => {}
        }
    }
}

fuzz_target!(|bytes: &[u8]| {
    let Some((&first, bytes)) = bytes.split_first() else {
        return;
    };
    let places = (first & 0b111) + 1;
    let whole = run(places, bytes, false);
    check(&whole);
    assert_eq!(run(places, bytes, true), whole, "the two runs differ");
});
