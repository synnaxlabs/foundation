use proptest::prelude::*;
use types::frame::{BadEnd, Path, Range};
use types::sample::{Scalar, Type};

use super::*;
use crate::hub::tests::{encode_ends, key};

/// The index (place 0), then an `I64` (place 1) and an `F64` (place 2).
fn create_set() -> Arc<KeySet> {
    create_set_of(3)
}

/// The index (place 0), then `places - 1` places, each an `I64` but place 2, an
/// `F64`.
fn create_set_of(places: u32) -> Arc<KeySet> {
    let data = |n: u32| match n {
        1 => Type::Scalar(Scalar::Stamp),
        3 => Type::Scalar(Scalar::F64),
        _ => Type::Scalar(Scalar::I64),
    };
    let channels = (1..=places).map(|n| (key(u128::from(n)), data(n), 0));
    Arc::new(KeySet::ordered(channels).expect("no key repeats"))
}

fn create_pool() -> block::Pool {
    let config = block::Config::new(1 << 16).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

fn encode_head(time: Time, form: Form, count: u32, series: u32) -> Vec<u8> {
    let mut out = vec![0xaa; Head::LEN];
    Head {
        time,
        form,
        count,
        series,
    }
    .encode(&mut out);
    out
}

/// A head's bytes, with each field as given.
fn head_bytes(time: u8, form: u8, count: u32, series: u32) -> Vec<u8> {
    let mut bytes = vec![7, time, form];
    bytes.extend(count.to_le_bytes());
    bytes.extend(series.to_le_bytes());
    bytes
}

const LIVE: Time = Time::Frame(Path::Live);

/// Gives `frames` the draft that is due, in the form that it is due in.
fn draft(frames: &mut Frames, pool: &block::Pool) -> Step {
    let Ok(Due::Draft { layout, form }) = frames.due() else {
        panic!("a draft is due");
    };
    let draft = layout.draft(pool, form).expect("a block");
    frames.draft(draft)
}

/// Takes a live head of 2 samples and the ends of the index and place 1.
fn ends(frames: &mut Frames) {
    let step = frames.decode(&encode_head(LIVE, Form::Raw, 2, 2));
    assert!(matches!(step, Ok(Step::Head(head)) if head.series == 2));
    assert!(matches!(
        frames.decode(&encode_ends(&[(0, 16)])),
        Ok(Step::More)
    ));
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert!(matches!(
        frames.decode(&encode_ends(&[(1, 32)])),
        Ok(Step::More)
    ));
}

/// The 32 bytes of a body: 2 stamps, then 2 samples of place 1.
fn body() -> Vec<u8> {
    (0..32).collect()
}

/// The frame of `step`, with its time.
fn frame(step: Step) -> (types::frame::Frame, Time) {
    let Step::Frame { draft, time } = step else {
        panic!("a frame, not {step:?}");
    };
    (draft.freeze(Path::Live), time)
}

fn check(frame: &types::frame::Frame, body: &[u8]) {
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 2 }));
    assert_eq!(frame.series(0), Some(&body[..16]));
    assert_eq!(frame.series(1), Some(&body[16..]));
    assert_eq!(frame.series(2), None);
}

/// The bytes of the ends of each place: an entry and an end, each a `usize`.
#[test]
fn holds_two_usizes_for_each_place() {
    let place = 2 * size_of::<usize>();
    let bytes = [0, 1, 3].map(Frames::bytes);
    assert_eq!(bytes, [0, place, 3 * place]);
}

#[test]
fn builds_a_frame_from_a_whole_body_message() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    let (frame, time) = frame(frames.decode(&body()).expect("the body"));
    assert_eq!(time, LIVE);
    check(&frame, &body());
    assert!(matches!(frames.due(), Ok(Due::Message)));
}

#[test]
fn finishes_between_frames() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    assert_eq!(frames.finish(), Ok(()));
    ends(&mut frames);
    draft(&mut frames, &pool);
    frames.decode(&body()).expect("the body");
    assert_eq!(frames.finish(), Ok(()));
}

#[test]
fn gives_the_bytes_of_a_frame_that_the_stream_cuts() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    let cut = |received, len| Err(Error::Cut { received, len });
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2, 2))
        .expect("the head");
    assert_eq!(frames.finish(), cut(11, None));
    frames.decode(&encode_ends(&[(0, 16)])).expect("the end");
    assert_eq!(frames.finish(), cut(19, None));
    frames.decode(&encode_ends(&[(1, 32)])).expect("the end");
    draft(&mut frames, &pool);
    assert_eq!(frames.finish(), cut(27, NonZeroUsize::new(59)));
    frames.decode(&body()[..20]).expect("the part");
    assert_eq!(frames.finish(), cut(47, NonZeroUsize::new(59)));
}

#[test]
fn prints_each_cut() {
    let cases = [
        (
            NonZeroUsize::new(59),
            "the stream ended inside a frame: 47 of 59 bytes",
        ),
        (
            None,
            "the stream ended inside a frame: 47 bytes, before the end of its ends",
        ),
    ];
    for (len, text) in cases {
        let cut = Error::Cut { received: 47, len };
        assert_eq!(cut.to_string(), text);
    }
}

#[test]
#[should_panic(expected = "a draft is due before the end of the stream")]
fn panics_on_a_finish_when_a_draft_is_due() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    let _finished = frames.finish();
}

#[test]
#[should_panic(expected = "a draft is due before the next message")]
fn panics_on_a_message_when_a_draft_is_due() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    let _step = frames.decode(&body());
}

#[test]
#[should_panic(expected = "a body is due")]
fn panics_on_a_body_received_in_place_when_a_draft_is_due() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    let _step = frames.received(1);
}

#[test]
#[should_panic(expected = "no draft is due")]
fn panics_on_a_draft_when_no_draft_is_due() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    let Ok(Due::Draft { layout, form }) = frames.due() else {
        panic!("a draft is due");
    };
    let twice = layout.draft(&pool, form).expect("a block");
    draft(&mut frames, &pool);
    let _step = frames.draft(twice);
}

#[test]
fn builds_a_frame_from_a_body_received_in_place() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    draft(&mut frames, &pool);
    let body = body();
    for part in body.chunks(10) {
        let Ok(Due::Body(rest)) = frames.due() else {
            panic!("a body is due");
        };
        rest[..part.len()].copy_from_slice(part);
        let step = frames.received(part.len()).expect("the part");
        if part.as_ptr_range().end == body.as_ptr_range().end {
            let (frame, _) = frame(step);
            check(&frame, &body);
        } else {
            assert!(matches!(step, Step::More));
        }
    }
}

#[test]
fn gives_a_frame_of_no_body_at_its_draft() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1, 1))
        .expect("the head");
    frames.decode(&encode_ends(&[(0, 0)])).expect("the ends");
    let (frame, _) = frame(draft(&mut frames, &pool));
    assert_eq!(frame.series(0), Some(&[][..]));
    assert!(matches!(frames.due(), Ok(Due::Message)));
}

#[test]
fn gives_the_time_and_form_of_each_head() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    let cases = [
        (Time::Node, Form::Raw, 1),
        (Time::Frame(Path::Backfill), Form::Encoded, 3),
        (LIVE, Form::Raw, 2),
    ];
    for (time, form, count) in cases {
        frames
            .decode(&encode_head(time, form, count, 1))
            .expect("the head");
        frames.decode(&encode_ends(&[(0, 8)])).expect("the ends");
        let Ok(Due::Draft { form: due, .. }) = frames.due() else {
            panic!("a draft is due");
        };
        assert_eq!(due, form);
        draft(&mut frames, &pool);
        let step = frames.decode(&[0; 8]).expect("the body");
        let Step::Frame { draft, time: given } = step else {
            panic!("a frame, not {step:?}");
        };
        assert_eq!(given, time);
        assert_eq!(draft.form(), form);
        assert_eq!(draft.range(0), Some(Range { seq: 0, count }));
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each error")]
fn refuses_each_head_that_the_program_cannot_encode() {
    let cases = [
        (vec![], Error::Empty, "the hub message is empty"),
        (
            vec![1, 0],
            Error::Kind { kind: 1 },
            "the hub message has kind 1, which this stream does not carry",
        ),
        (
            head_bytes(0, 0, 1, 1)[..10].to_vec(),
            Error::Length { len: 10 },
            "the hub message has 10 bytes, which no message of its kind has",
        ),
        (
            [head_bytes(0, 0, 1, 1), vec![0]].concat(),
            Error::Length { len: 12 },
            "the hub message has 12 bytes, which no message of its kind has",
        ),
        (
            head_bytes(3, 2, 0, 0),
            Error::Path { byte: 3 },
            "the frame head names path or time 3, which this node does not know",
        ),
        (
            head_bytes(0, 2, 0, 0),
            Error::Form { byte: 2 },
            "the frame head names form 2, which no frame has",
        ),
        (
            head_bytes(2, 1, 0, 0),
            Error::Samples,
            "the frame head holds no sample",
        ),
        (
            head_bytes(2, 1, 2, 0),
            Error::Series,
            "the frame head names no series",
        ),
        (
            head_bytes(2, 1, 1, 9),
            Error::Stamp {
                count: 1,
                form: Form::Encoded,
            },
            "the frame head that the node stamps has 1 samples in the Encoded form, \
             not 1 in the raw form",
        ),
        (
            head_bytes(2, 0, 2, 9),
            Error::Stamp {
                count: 2,
                form: Form::Raw,
            },
            "the frame head that the node stamps has 2 samples in the Raw form, not 1 \
             in the raw form",
        ),
        (
            head_bytes(0, 1, 2, 4),
            Error::Places {
                series: 4,
                places: 3,
            },
            "the frame head names 4 series, and the session has 3 places",
        ),
    ];
    let mut frames = Frames::new(create_set());
    for (message, error, text) in cases {
        let decoded = frames.decode(&message);
        assert_eq!(decoded.map(|_| ()), Err(error), "{message:?}");
        assert_eq!(error.to_string(), text);
    }
    let step = frames.decode(&encode_head(Time::Node, Form::Raw, 1, 3));
    assert!(matches!(step, Ok(Step::Head(head)) if head.series == 3));
}

#[test]
fn refuses_a_run_message_of_more_ends_than_remain_and_keeps_the_run() {
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1, 1))
        .expect("the head");
    let decoded = frames.decode(&encode_ends(&[(0, 8), (1, 16)]));
    assert_eq!(
        decoded.map(|_| ()),
        Err(Error::Run {
            items: 2,
            remain: 1
        })
    );
    assert!(matches!(
        frames.decode(&encode_ends(&[(0, 8)])),
        Ok(Step::More)
    ));
}

#[test]
fn refuses_an_empty_or_long_body_message_and_keeps_the_body() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    draft(&mut frames, &pool);
    assert_eq!(frames.decode(&[]).map(|_| ()), Err(Error::Empty));
    assert_eq!(frames.received(0).map(|_| ()), Err(Error::Empty));
    assert_eq!(
        frames.decode(&[0; 33]).map(|_| ()),
        Err(Error::Body {
            len: 33,
            remain: 32
        })
    );
    let (frame, _) = frame(frames.decode(&body()).expect("the body"));
    check(&frame, &body());
}

#[test]
fn gives_the_layout_error_of_ends_out_of_order() {
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 16), (1, 8)]))
        .expect("the ends");
    assert!(matches!(
        frames.due(),
        Err(types::frame::Error::End(BadEnd::Before { .. }))
    ));
}

/// Takes drafts of the due layout from `pool` until it has none, and gives them with
/// the pool's error.
fn create_full(frames: &mut Frames, pool: &block::Pool) -> (Vec<Draft>, block::Error) {
    let Ok(Due::Draft { layout, form }) = frames.due() else {
        panic!("a draft is due");
    };
    let mut held = Vec::new();
    loop {
        match layout.draft(pool, form) {
            Ok(draft) => held.push(draft),
            Err(error) => return (held, error),
        }
    }
}

#[test]
fn takes_the_next_frame_after_a_frame_of_no_body_finds_the_pool_full() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1, 1))
        .expect("the head");
    frames.decode(&encode_ends(&[(0, 0)])).expect("the ends");
    let (held, error) = create_full(&mut frames, &pool);
    assert!(matches!(error, block::Error::Exhausted { .. }), "{error:?}");
    frames.skip();
    assert!(frames.head.is_none(), "the skipped frame ended");
    drop(held);
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert_eq!(frames.finish(), Ok(()));
    ends(&mut frames);
    draft(&mut frames, &pool);
    let (frame, _) = frame(frames.decode(&body()).expect("the body"));
    check(&frame, &body());
}

#[test]
fn drops_the_body_of_a_skipped_frame_and_takes_the_next_frame() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    let (held, _) = create_full(&mut frames, &pool);
    frames.skip();
    drop(held);
    let body = body();
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert!(matches!(frames.decode(&body[..20]), Ok(Step::More)));
    assert_eq!(
        frames.finish(),
        Err(Error::Cut {
            received: 47,
            len: NonZeroUsize::new(59)
        })
    );
    assert!(matches!(frames.decode(&body[20..]), Ok(Step::More)));
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert_eq!(frames.finish(), Ok(()));
    ends(&mut frames);
    draft(&mut frames, &pool);
    let (frame, _) = frame(frames.decode(&body).expect("the body"));
    check(&frame, &body);
}

#[test]
fn refuses_an_empty_or_long_body_message_of_a_skipped_frame() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    frames.skip();
    assert_eq!(frames.decode(&[]).map(|_| ()), Err(Error::Empty));
    assert_eq!(
        frames.decode(&[0; 33]).map(|_| ()),
        Err(Error::Body {
            len: 33,
            remain: 32
        })
    );
    assert!(matches!(frames.decode(&body()), Ok(Step::More)));
}

#[test]
#[should_panic(expected = "no draft is due")]
fn panics_on_a_skip_when_no_draft_is_due() {
    let mut frames = Frames::new(create_set());
    frames.skip();
}

/// Frames after ends whose layout is not valid, with the layout error given.
fn create_refused() -> Frames {
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 16), (1, 8)]))
        .expect("the ends");
    assert!(frames.due().is_err(), "the layout is not valid");
    frames
}

#[test]
#[should_panic(expected = "no draft is due")]
fn panics_on_a_skip_after_a_layout_error() {
    create_refused().skip();
}

#[test]
#[should_panic(expected = "no draft is due")]
fn panics_on_a_draft_after_a_layout_error() {
    let pool = create_pool();
    let mut other = Frames::new(create_set());
    ends(&mut other);
    let Ok(Due::Draft { layout, form }) = other.due() else {
        panic!("a draft is due");
    };
    let draft = layout.draft(&pool, form).expect("a block");
    let _step = create_refused().draft(draft);
}

#[test]
#[should_panic(expected = "a body is due")]
fn panics_on_a_body_received_in_place_for_a_skipped_frame() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    frames.skip();
    let _received = frames.received(1);
}

#[test]
#[should_panic(expected = "a write stream is on one index")]
fn panics_for_a_set_of_two_groups() {
    let set = types::frame::key_set::Interner::new().intern(&[
        types::frame::key_set::Group {
            index: key(1),
            data: &[],
        },
        types::frame::key_set::Group {
            index: key(2),
            data: &[],
        },
    ]);
    drop(Frames::new(set));
}

/// A frame of `count` samples of the `present` places, the bytes of each of its
/// series, and the sizes to cut its ends and body into.
#[derive(Debug)]
struct Case {
    places: u32,
    count: u32,
    present: Vec<u32>,
    series: Vec<Vec<u8>>,
    cuts: Vec<usize>,
    in_place: bool,
}

fn case() -> impl Strategy<Value = Case> {
    (1u32..=4, 1usize..=4)
        .prop_flat_map(|(places, count)| {
            let data: Vec<u32> = (1..places).collect();
            let len = data.len();
            let data = proptest::sample::subsequence(data, 0..=len);
            (Just(places), Just(count), data)
        })
        .prop_flat_map(|(places, count, data)| {
            let present: Vec<u32> = std::iter::once(0).chain(data).collect();
            let sample = proptest::collection::vec(any::<[u8; 8]>(), count);
            let series = proptest::collection::vec(
                sample.prop_map(|samples| samples.concat()),
                present.len(),
            );
            (
                Just(places),
                Just(u32::try_from(count).expect("a u32 holds 4")),
                Just(present),
                series,
                proptest::collection::vec(1usize..=64, 1..8),
                any::<bool>(),
            )
        })
        .prop_map(|(places, count, present, series, cuts, in_place)| Case {
            places,
            count,
            present,
            series,
            cuts,
            in_place,
        })
}

/// The ends of `case`: each present place and the end of its series.
fn ends_of(case: &Case) -> Vec<(u32, u32)> {
    let mut end = 0u32;
    case.present
        .iter()
        .zip(&case.series)
        .map(|(&place, bytes)| {
            let len = u32::try_from(bytes.len()).expect("a u32 holds the series");
            end = end.checked_add(len).expect("the body fits a u32");
            (place, end)
        })
        .collect()
}

proptest! {
    #[test]
    fn builds_each_frame_from_its_messages_cut_at_random(case in case()) {
        let pool = create_pool();
        let mut frames = Frames::new(create_set_of(case.places));
        let ends = ends_of(&case);
        let body = case.series.concat();
        let series = u32::try_from(case.present.len()).expect("at most 4");
        // Twice, so that the frames take a second frame after the first.
        for _ in 0..2 {
            let step = frames.decode(&encode_head(LIVE, Form::Raw, case.count, series));
            prop_assert!(matches!(step, Ok(Step::Head(_))));
            let mut cuts = case.cuts.iter().copied().cycle();
            for run in crate::hub::tests::cut(&ends, &mut cuts) {
                let step = frames.decode(&encode_ends(run));
                prop_assert!(matches!(step, Ok(Step::More)));
            }
            let mut step = draft(&mut frames, &pool);
            let fit = "the bytes of a frame fit a usize";
            let run = ends::LEN.checked_mul(ends.len()).expect(fit);
            let mut received = Head::LEN.checked_add(run).expect(fit);
            let len = NonZeroUsize::new(received.checked_add(body.len()).expect(fit));
            for part in crate::hub::tests::cut(&body, &mut cuts) {
                prop_assert_eq!(frames.finish(), Err(Error::Cut { received, len }));
                received = received.checked_add(part.len()).expect(fit);
                step = if case.in_place {
                    let Ok(Due::Body(rest)) = frames.due() else {
                        panic!("a body is due");
                    };
                    rest[..part.len()].copy_from_slice(part);
                    frames.received(part.len()).expect("the part")
                } else {
                    frames.decode(part).expect("the part")
                };
            }
            prop_assert_eq!(frames.finish(), Ok(()));
            let (frame, _) = frame(step);
            prop_assert_eq!(frame.range(0), Some(Range { seq: 0, count: case.count }));
            for (&place, bytes) in case.present.iter().zip(&case.series) {
                prop_assert_eq!(frame.series(to_usize(place)), Some(&bytes[..]));
            }
        }
    }
}
