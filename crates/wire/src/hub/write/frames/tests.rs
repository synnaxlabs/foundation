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

/// The index of group 0 (place 0), an `I64` of it (place 1), the index of group 1
/// (place 2), and an `F64` of it (place 3).
fn create_two() -> Arc<KeySet> {
    let channels = [
        (key(1), Type::Scalar(Scalar::Stamp), 0),
        (key(2), Type::Scalar(Scalar::I64), 0),
        (key(3), Type::Scalar(Scalar::Stamp), 2),
        (key(4), Type::Scalar(Scalar::F64), 2),
    ];
    Arc::new(KeySet::ordered(channels).expect("no key repeats"))
}

fn encode_head(time: Time, form: Form, series: u32) -> Vec<u8> {
    let mut out = vec![0xaa; Head::LEN];
    Head { time, form, series }.encode(&mut out);
    out
}

fn encode_counts(values: &[u32]) -> Vec<u8> {
    let len = values
        .len()
        .checked_mul(counts::LEN)
        .expect("the counts fit a usize");
    let mut out = vec![0xaa; len];
    counts::encode(values.iter().copied(), &mut out);
    out
}

/// A head's bytes, with each field as given.
fn head_bytes(time: u8, form: u8, series: u32) -> Vec<u8> {
    let mut bytes = vec![7, time, form];
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

/// Takes a live head and the ends of the index and place 1.
fn ends(frames: &mut Frames) {
    let step = frames.decode(&encode_head(LIVE, Form::Raw, 2));
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

/// Takes a count of 2, the run of counts of [`ends`].
fn count(frames: &mut Frames) {
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert!(matches!(
        frames.decode(&encode_counts(&[2])),
        Ok(Step::More)
    ));
}

/// Takes the messages of [`ends`], a draft from `pool`, and [`count`].
fn lead(frames: &mut Frames, pool: &block::Pool) {
    ends(frames);
    assert!(matches!(draft(frames, pool), Step::More));
    count(frames);
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

/// The bytes of each place: an entry and an end, each a `usize`, and a count.
#[test]
fn holds_two_usizes_and_a_count_for_each_place() {
    let place = 2 * size_of::<usize>() + counts::LEN;
    let bytes = [0, 1, 3].map(Frames::bytes);
    assert_eq!(bytes, [0, place, 3 * place]);
}

#[test]
fn builds_a_frame_from_a_whole_body_message() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    lead(&mut frames, &pool);
    let (frame, time) = frame(frames.decode(&body()).expect("the body"));
    assert_eq!(time, LIVE);
    check(&frame, &body());
    assert!(matches!(frames.due(), Ok(Due::Message)));
}

#[test]
fn finishes_between_frames() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    assert!(frames.between());
    assert_eq!(frames.finish(), Ok(()));
    lead(&mut frames, &pool);
    assert!(!frames.between());
    frames.decode(&body()).expect("the body");
    assert!(frames.between());
    assert_eq!(frames.finish(), Ok(()));
}

#[test]
fn gives_the_bytes_of_a_frame_that_the_stream_cuts() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    let cut = |received, len| Err(Error::Cut { received, len });
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    assert_eq!(frames.finish(), cut(7, None));
    frames.decode(&encode_ends(&[(0, 16)])).expect("the end");
    assert_eq!(frames.finish(), cut(15, None));
    frames.decode(&encode_ends(&[(1, 32)])).expect("the end");
    draft(&mut frames, &pool);
    assert_eq!(frames.finish(), cut(23, NonZeroUsize::new(59)));
    frames.decode(&encode_counts(&[2])).expect("the count");
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
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    let _step = frames.draft(twice);
}

#[test]
fn builds_a_frame_from_a_body_received_in_place() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    lead(&mut frames, &pool);
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
fn gives_a_frame_of_no_body_at_its_last_count() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 1))
        .expect("the head");
    frames.decode(&encode_ends(&[(0, 0)])).expect("the ends");
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert_eq!(
        frames.finish(),
        Err(Error::Cut {
            received: 15,
            len: NonZeroUsize::new(19)
        })
    );
    let step = frames.decode(&encode_counts(&[1])).expect("the count");
    let (frame, _) = frame(step);
    assert_eq!(frame.series(0), Some(&[][..]));
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 1 }));
    assert!(matches!(frames.due(), Ok(Due::Message)));
}

#[test]
fn gives_a_frame_of_no_body_that_the_node_stamps_at_its_draft() {
    let pool = create_pool();
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(Time::Node, Form::Raw, 1))
        .expect("the head");
    frames.decode(&encode_ends(&[(0, 0)])).expect("the ends");
    let (frame, time) = frame(draft(&mut frames, &pool));
    assert_eq!(time, Time::Node);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 1 }));
    assert_eq!(frames.finish(), Ok(()));
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
            .decode(&encode_head(time, form, 1))
            .expect("the head");
        frames.decode(&encode_ends(&[(0, 8)])).expect("the ends");
        let Ok(Due::Draft { form: due, .. }) = frames.due() else {
            panic!("a draft is due");
        };
        assert_eq!(due, form);
        draft(&mut frames, &pool);
        if time != Time::Node {
            frames.decode(&encode_counts(&[count])).expect("the count");
        }
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
fn refuses_each_head_that_the_program_cannot_encode() {
    let cases = [
        (vec![], Error::Empty, "the hub message is empty"),
        (
            vec![1, 0],
            Error::Kind { kind: 1 },
            "the hub message has kind 1, which this stream does not carry",
        ),
        (
            head_bytes(0, 0, 1)[..6].to_vec(),
            Error::Length { len: 6 },
            "the hub message has 6 bytes, which no message of its kind has",
        ),
        (
            [head_bytes(0, 0, 1), vec![0]].concat(),
            Error::Length { len: 8 },
            "the hub message has 8 bytes, which no message of its kind has",
        ),
        (
            head_bytes(3, 2, 0),
            Error::Path { byte: 3 },
            "the frame head names path or time 3, which this node does not know",
        ),
        (
            head_bytes(0, 2, 0),
            Error::Form { byte: 2 },
            "the frame head names form 2, which no frame has",
        ),
        (
            head_bytes(2, 1, 0),
            Error::Series,
            "the frame head names no series",
        ),
        (
            head_bytes(2, 1, 9),
            Error::Stamp,
            "the frame head that the node stamps is in the encoded form, not the raw \
             form",
        ),
        (
            head_bytes(0, 1, 4),
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
    let step = frames.decode(&encode_head(Time::Node, Form::Raw, 3));
    assert!(matches!(step, Ok(Step::Head(head)) if head.series == 3));
}

#[test]
fn refuses_a_run_message_of_more_ends_than_remain_and_keeps_the_run() {
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(Time::Node, Form::Raw, 1))
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
    lead(&mut frames, &pool);
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
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 16), (1, 8)]))
        .expect("the ends");
    assert!(matches!(
        frames.due(),
        Err(types::frame::Error::End(BadEnd::Before { .. }))
    ));
}

#[test]
fn gives_the_layout_error_of_a_repeated_index() {
    let mut frames = Frames::new(create_set());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 8), (0, 16)]))
        .expect("the ends");
    assert!(matches!(
        frames.due(),
        Err(types::frame::Error::Unordered { entry: 0, last: 0 })
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
        .decode(&encode_head(LIVE, Form::Raw, 1))
        .expect("the head");
    frames.decode(&encode_ends(&[(0, 0)])).expect("the ends");
    let (held, error) = create_full(&mut frames, &pool);
    assert!(matches!(error, block::Error::Exhausted { .. }), "{error:?}");
    frames.skip();
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert!(matches!(
        frames.decode(&encode_counts(&[1])),
        Ok(Step::More)
    ));
    assert!(frames.head.is_none(), "the skipped frame ended");
    drop(held);
    assert!(matches!(frames.due(), Ok(Due::Message)));
    assert_eq!(frames.finish(), Ok(()));
    lead(&mut frames, &pool);
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
    count(&mut frames);
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
    lead(&mut frames, &pool);
    let (frame, _) = frame(frames.decode(&body).expect("the body"));
    check(&frame, &body);
}

#[test]
fn refuses_an_empty_or_long_body_message_of_a_skipped_frame() {
    let mut frames = Frames::new(create_set());
    ends(&mut frames);
    frames.skip();
    assert_eq!(frames.decode(&[0; 4]).map(|_| ()), Err(Error::Samples));
    count(&mut frames);
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
        .decode(&encode_head(LIVE, Form::Raw, 2))
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
    count(&mut frames);
    let _received = frames.received(1);
}

/// Takes a head of `time` and the ends of each place of [`create_two`], with 8 bytes
/// for each.
fn two_ends(frames: &mut Frames, time: Time) {
    frames
        .decode(&encode_head(time, Form::Raw, 4))
        .expect("the head");
    let ends = encode_ends(&[(0, 8), (1, 16), (2, 24), (3, 32)]);
    frames.decode(&ends).expect("the ends");
}

/// The frame of `frames` after a body of 32 bytes.
fn two_frame(frames: &mut Frames) -> types::frame::Frame {
    frame(frames.decode(&[0; 32]).expect("the body")).0
}

#[test]
fn sets_the_count_of_each_group_from_the_run_of_counts() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    two_ends(&mut frames, LIVE);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert!(matches!(frames.due(), Ok(Due::Message)));
    frames
        .decode(&encode_counts(&[1]))
        .expect("the first count");
    assert_eq!(
        frames.finish(),
        Err(Error::Cut {
            received: 43,
            len: NonZeroUsize::new(79)
        })
    );
    assert!(matches!(frames.due(), Ok(Due::Message)));
    frames
        .decode(&encode_counts(&[3]))
        .expect("the second count");
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 1 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 3 }));
}

#[test]
fn counts_only_the_groups_whose_index_the_ends_hold() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(2, 8), (3, 16)]))
        .expect("the ends");
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert_eq!(
        frames.decode(&encode_counts(&[1, 1])).map(|_| ()),
        Err(Error::Run {
            items: 2,
            remain: 1
        })
    );
    frames.decode(&encode_counts(&[2])).expect("the count");
    let (frame, _) = frame(frames.decode(&[0; 16]).expect("the body"));
    assert_eq!(frame.range(0), None);
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 2 }));
}

#[test]
fn gives_each_group_of_a_head_that_the_node_stamps_one_sample() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    two_ends(&mut frames, Time::Node);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert_eq!(
        frames.finish(),
        Err(Error::Cut {
            received: 39,
            len: NonZeroUsize::new(71)
        })
    );
    let (frame, _) = frame(frames.decode(&[0; 32]).expect("the body"));
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 1 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 1 }));
}

#[test]
fn refuses_each_run_of_counts_that_the_program_cannot_encode_and_keeps_it() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    two_ends(&mut frames, LIVE);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    let cases = [
        (vec![], Error::Empty),
        (vec![1, 0, 0], Error::Length { len: 3 }),
        (
            encode_counts(&[1, 1, 1]),
            Error::Run {
                items: 3,
                remain: 2,
            },
        ),
        (
            vec![0; 12],
            Error::Run {
                items: 3,
                remain: 2,
            },
        ),
        ([encode_counts(&[1]), vec![0; 4]].concat(), Error::Samples),
    ];
    for (message, error) in cases {
        let decoded = frames.decode(&message);
        assert_eq!(decoded.map(|_| ()), Err(error), "{message:?}");
    }
    assert_eq!(
        Error::Samples.to_string(),
        "the frame holds a group of no sample"
    );
    frames.decode(&encode_counts(&[2, 4])).expect("the counts");
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 2 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 4 }));
}

/// Takes a live head and the ends of [`two_ends`], with `counts` after the last end.
fn two_ends_and_counts(frames: &mut Frames, counts: &[u32]) {
    frames
        .decode(&encode_head(LIVE, Form::Raw, 4))
        .expect("the head");
    let ends = encode_ends(&[(0, 8), (1, 16), (2, 24), (3, 32)]);
    let message = [ends, encode_counts(counts)].concat();
    frames.decode(&message).expect("the ends and the counts");
}

#[test]
fn sets_the_counts_after_the_last_end_at_the_draft() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    two_ends_and_counts(&mut frames, &[1]);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert_eq!(
        frames.finish(),
        Err(Error::Cut {
            received: 43,
            len: NonZeroUsize::new(79)
        })
    );
    frames
        .decode(&encode_counts(&[3]))
        .expect("the second count");
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 1 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 3 }));
}

#[test]
fn counts_the_indexes_of_each_message_of_the_run_of_ends() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 4))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 8), (1, 16)]))
        .expect("the first ends");
    let last = [encode_ends(&[(2, 24), (3, 32)]), encode_counts(&[2, 4])].concat();
    frames.decode(&last).expect("the last ends and the counts");
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert!(matches!(frames.due(), Ok(Due::Body(_))));
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 2 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 4 }));
}

#[test]
fn refuses_each_run_of_counts_after_the_last_end_and_keeps_the_ends() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 4))
        .expect("the head");
    let ends = encode_ends(&[(0, 8), (1, 16), (2, 24), (3, 32)]);
    let cases = [
        (vec![1, 0, 0], Error::Length { len: 3 }),
        (
            encode_counts(&[1, 1, 1]),
            Error::Run {
                items: 3,
                remain: 2,
            },
        ),
        ([encode_counts(&[1]), vec![0; 4]].concat(), Error::Samples),
    ];
    for (counts, error) in cases {
        let decoded = frames.decode(&[ends.clone(), counts].concat());
        assert_eq!(decoded.map(|_| ()), Err(error));
    }
    frames.decode(&ends).expect("the ends");
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert!(matches!(frames.due(), Ok(Due::Message)));
    frames.decode(&encode_counts(&[2, 4])).expect("the counts");
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 2 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 4 }));
}

#[test]
fn refuses_bytes_after_the_last_end_of_a_head_that_the_node_stamps() {
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(Time::Node, Form::Raw, 4))
        .expect("the head");
    let ends = encode_ends(&[(0, 8), (1, 16), (2, 24), (3, 32)]);
    let decoded = frames.decode(&[ends.clone(), encode_counts(&[1, 1])].concat());
    assert_eq!(
        decoded.map(|_| ()),
        Err(Error::Run {
            items: 5,
            remain: 4
        })
    );
    assert!(matches!(frames.decode(&ends), Ok(Step::More)));
}

#[test]
fn drops_the_counts_after_the_last_end_of_a_skipped_frame() {
    let pool = create_pool();
    let mut frames = Frames::new(create_two());
    two_ends_and_counts(&mut frames, &[1, 3]);
    assert!(matches!(frames.due(), Ok(Due::Draft { .. })));
    frames.skip();
    assert!(matches!(frames.decode(&[0; 32]), Ok(Step::More)));
    assert_eq!(frames.finish(), Ok(()));
    two_ends(&mut frames, LIVE);
    assert!(matches!(draft(&mut frames, &pool), Step::More));
    assert!(matches!(frames.due(), Ok(Due::Message)));
    frames.decode(&encode_counts(&[2, 4])).expect("the counts");
    let frame = two_frame(&mut frames);
    assert_eq!(frame.range(0), Some(Range { seq: 0, count: 2 }));
    assert_eq!(frame.range(1), Some(Range { seq: 0, count: 4 }));
}

#[test]
fn refuses_a_series_of_a_group_whose_index_the_ends_do_not_hold() {
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(0, 8), (3, 16)]))
        .expect("the ends");
    assert!(matches!(
        frames.due(),
        Err(types::frame::Error::IndexAbsent { entry: 3, index: 2 })
    ));
}

#[test]
fn gives_the_layout_error_of_ends_with_no_index() {
    let mut frames = Frames::new(create_two());
    frames
        .decode(&encode_head(LIVE, Form::Raw, 2))
        .expect("the head");
    frames
        .decode(&encode_ends(&[(1, 8), (9, 16)]))
        .expect("the ends");
    assert!(matches!(
        frames.due(),
        Err(types::frame::Error::OutOfRange { entry: 9, .. })
    ));
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

/// Whether [`Frames::between`] and [`Frames::finish`] agree on `frames`, or a draft is
/// due, where `finish` panics.
fn agree(frames: &Frames) -> bool {
    frames.assembly.drafting() || frames.between() == frames.finish().is_ok()
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
            prop_assert!(frames.between());
            let step = frames.decode(&encode_head(LIVE, Form::Raw, series));
            prop_assert!(matches!(step, Ok(Step::Head(_))));
            prop_assert!(agree(&frames));
            let mut cuts = case.cuts.iter().copied().cycle();
            for run in crate::hub::tests::cut(&ends, &mut cuts) {
                let step = frames.decode(&encode_ends(run));
                prop_assert!(matches!(step, Ok(Step::More)));
                prop_assert!(agree(&frames));
            }
            prop_assert!(matches!(draft(&mut frames, &pool), Step::More));
            prop_assert!(agree(&frames));
            let mut step = frames
                .decode(&encode_counts(&[case.count]))
                .expect("the count");
            prop_assert!(agree(&frames));
            let fit = "the bytes of a frame fit a usize";
            let run = ends::LEN
                .checked_mul(ends.len())
                .and_then(|n| n.checked_add(counts::LEN));
            let run = run.expect(fit);
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
                prop_assert!(agree(&frames));
            }
            prop_assert!(frames.between());
            prop_assert_eq!(frames.finish(), Ok(()));
            let (frame, _) = frame(step);
            prop_assert_eq!(frame.range(0), Some(Range { seq: 0, count: case.count }));
            for (&place, bytes) in case.present.iter().zip(&case.series) {
                prop_assert_eq!(frame.series(to_usize(place)), Some(&bytes[..]));
            }
        }
    }
}

/// The inputs of the fuzz target `wire_hub_write` give the frames and ends that their
/// names state.
mod corpus {
    use super::*;

    /// How a stream of a fuzz input ends.
    #[derive(Debug, PartialEq)]
    enum End {
        /// The input ended, with what `finish` gave.
        Finished(Result<(), Error>),
        /// A message gave an error.
        Message(Error),
        /// A draft's layout gave an error.
        Layout(types::frame::Error),
    }

    /// The name of the input `$file` of the fuzz target `wire_hub_write`, and its
    /// bytes.
    macro_rules! corpus {
        ($file:literal) => {
            (
                $file,
                include_bytes!(concat!(
                    "../../../../../../oracles/fuzz/wire_hub_write/",
                    $file
                ))
                .as_slice(),
            )
        };
    }

    /// The set of the first byte of an input: an index, then `places - 1` channels of
    /// `I64`, where bit 3 makes the second half of the places a second group.
    fn create_input_set(first: u8) -> Arc<KeySet> {
        let places = usize::from(first & 0b111) + 1;
        let split = if first & 0b1000 != 0 && places > 1 {
            places.div_ceil(2)
        } else {
            usize::MAX
        };
        let channels = (0..places).map(|entry| {
            let index = if entry >= split { split } else { 0 };
            let data = if entry == index {
                Scalar::Stamp
            } else {
                Scalar::I64
            };
            let n = u128::try_from(entry)
                .expect("at most 8 places")
                .strict_add(1);
            (key(n), Type::Scalar(data), index)
        });
        Arc::new(KeySet::ordered(channels).expect("a valid set"))
    }

    /// The ranges of each frame of a stream, `None` for a frame skipped on a full pool.
    type Given = Vec<Option<Vec<(u32, Range)>>>;

    /// The ranges of each frame that the fuzz input `bytes` gives, and how its stream
    /// ends.
    fn drive(bytes: &[u8]) -> (Given, End) {
        let (&first, mut bytes) = bytes.split_first().expect("the input has a byte");
        let pool = create_pool();
        let mut frames = Frames::new(create_input_set(first));
        let mut seen = Vec::new();
        loop {
            let step = match frames.due() {
                Err(error) => return (seen, End::Layout(error)),
                Ok(Due::Draft { layout, form }) => {
                    let Ok(draft) = layout.draft(&pool, form) else {
                        frames.skip();
                        seen.push(None);
                        continue;
                    };
                    frames.draft(draft)
                }
                Ok(Due::Body(_) | Due::Message) => {
                    let Some((&len, rest)) = bytes.split_first() else {
                        return (seen, End::Finished(frames.finish()));
                    };
                    let (message, rest) =
                        rest.split_at(usize::from(len).min(rest.len()));
                    bytes = rest;
                    match frames.decode(message) {
                        Ok(step) => step,
                        Err(error) => return (seen, End::Message(error)),
                    }
                }
            };
            if let Step::Frame { draft, .. } = step {
                seen.push(Some(draft.ranges().collect()));
            }
        }
    }

    /// The ranges of a frame: each group, and its count from seq 0.
    fn ranges(groups: &[(u32, u32)]) -> Vec<(u32, Range)> {
        let ranges = groups
            .iter()
            .map(|&(group, count)| (group, Range { seq: 0, count }));
        ranges.collect()
    }

    fn cut(received: usize, len: Option<usize>) -> End {
        End::Finished(Err(Error::Cut {
            received,
            len: len.and_then(NonZeroUsize::new),
        }))
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one case for each input")]
    fn gives_the_frames_and_end_of_each_input_of_a_run_of_counts() {
        let run = |items, remain| End::Message(Error::Run { items, remain });
        let done = || End::Finished(Ok(()));
        let one = |count| Some(ranges(&[(0, count)]));
        let both = Some(ranges(&[(0, 1), (1, 1)]));
        let cases = [
            (corpus!("body_cut_grouped"), vec![], cut(54, Some(59))),
            (
                corpus!("body_empty_grouped"),
                vec![],
                End::Message(Error::Empty),
            ),
            (
                corpus!("body_past_grouped"),
                vec![],
                End::Message(Error::Body {
                    len: 40,
                    remain: 32,
                }),
            ),
            (
                corpus!("counts_cut"),
                vec![],
                End::Message(Error::Length { len: 3 }),
            ),
            (corpus!("counts_end"), vec![], cut(23, Some(59))),
            (
                corpus!("counts_index_repeat"),
                vec![],
                End::Layout(types::frame::Error::Unordered { entry: 0, last: 0 }),
            ),
            (corpus!("counts_over_run"), vec![], run(2, 1)),
            (
                corpus!("counts_samples"),
                vec![],
                End::Message(Error::Samples),
            ),
            (
                corpus!("cut_node_body"),
                vec![None],
                cut(34, Some(100_991_015)),
            ),
            (corpus!("node_body_cut"), vec![], cut(28, Some(39))),
            (
                corpus!("ends_bad_grouped"),
                vec![],
                End::Message(Error::Length { len: 5 }),
            ),
            (
                corpus!("ends_cut_grouped"),
                vec![],
                End::Message(Error::Length { len: 7 }),
            ),
            (
                corpus!("ends_empty_grouped"),
                vec![],
                End::Message(Error::Empty),
            ),
            (corpus!("ends_over_run_grouped"), vec![], run(2, 1)),
            (corpus!("ends_whole_cut_grouped"), vec![], cut(15, None)),
            (
                corpus!("frame_backfill_encoded_grouped"),
                vec![one(2)],
                done(),
            ),
            (corpus!("frame_cut_grouped"), vec![one(2)], done()),
            (corpus!("frame_grouped"), vec![one(2)], done()),
            (corpus!("frame_no_body_grouped"), vec![one(1)], done()),
            (corpus!("frame_node_grouped"), vec![one(1)], done()),
            (
                corpus!("head_empty_grouped"),
                vec![],
                End::Message(Error::Empty),
            ),
            (
                corpus!("head_form_grouped"),
                vec![],
                End::Message(Error::Form { byte: 2 }),
            ),
            (
                corpus!("head_kind_grouped"),
                vec![],
                End::Message(Error::Kind { kind: 1 }),
            ),
            (
                corpus!("head_long_grouped"),
                vec![],
                End::Message(Error::Length { len: 8 }),
            ),
            (corpus!("head_only_grouped"), vec![], cut(7, None)),
            (
                corpus!("head_places_grouped"),
                vec![],
                End::Message(Error::Places {
                    series: 3,
                    places: 2,
                }),
            ),
            (
                corpus!("head_series_grouped"),
                vec![],
                End::Message(Error::Series),
            ),
            (
                corpus!("head_short_grouped"),
                vec![],
                End::Message(Error::Length { len: 6 }),
            ),
            (
                corpus!("head_stamp_form_grouped"),
                vec![],
                End::Message(Error::Stamp),
            ),
            (
                corpus!("head_time_grouped"),
                vec![],
                End::Message(Error::Path { byte: 3 }),
            ),
            (
                corpus!("index_absent_grouped"),
                vec![],
                End::Layout(types::frame::Error::IndexAbsent { entry: 1, index: 0 }),
            ),
            (
                corpus!("layout_before_grouped"),
                vec![],
                End::Layout(types::frame::Error::End(BadEnd::Before {
                    end: 8,
                    start: 16,
                })),
            ),
            (
                corpus!("pool_full_grouped"),
                vec![None],
                cut(75, Some(4_194_315)),
            ),
            (
                corpus!("skip_cut_grouped"),
                vec![None],
                cut(25_498, Some(98_331)),
            ),
            (corpus!("skip_frame_grouped"), vec![None, one(2)], done()),
            (corpus!("two_frames_grouped"), vec![one(2), one(1)], done()),
            (corpus!("two_groups"), vec![both.clone()], done()),
            (
                corpus!("two_groups_counts_after_ends"),
                vec![both.clone()],
                done(),
            ),
            (
                corpus!("two_groups_counts_after_ends_cut"),
                vec![],
                End::Message(Error::Length { len: 3 }),
            ),
            (
                corpus!("two_groups_counts_after_ends_over_run"),
                vec![],
                run(3, 2),
            ),
            (
                corpus!("two_groups_counts_after_ends_samples"),
                vec![],
                End::Message(Error::Samples),
            ),
            (
                corpus!("two_groups_counts_after_ends_split"),
                vec![both.clone()],
                done(),
            ),
            (corpus!("two_groups_counts_cut"), vec![], cut(43, Some(79))),
            (corpus!("two_groups_counts_over_run"), vec![], run(3, 2)),
            (
                corpus!("two_groups_counts_samples"),
                vec![],
                End::Message(Error::Samples),
            ),
            (
                corpus!("two_groups_counts_split"),
                vec![both.clone()],
                done(),
            ),
            (corpus!("two_groups_node"), vec![both.clone()], done()),
            (corpus!("two_groups_node_ends_over_run"), vec![], run(5, 4)),
            (
                corpus!("two_groups_second"),
                vec![Some(ranges(&[(1, 1)]))],
                done(),
            ),
            (
                corpus!("two_groups_skip"),
                vec![None],
                cut(812, Some(131_119)),
            ),
            (
                corpus!("two_groups_skip_counts_after_ends"),
                vec![None],
                cut(812, Some(131_119)),
            ),
            (
                corpus!("two_groups_two_frames"),
                vec![both.clone(), both],
                done(),
            ),
        ];
        for ((name, bytes), frames, end) in cases {
            assert_eq!(drive(bytes), (frames, end), "{name}");
        }
    }
}
