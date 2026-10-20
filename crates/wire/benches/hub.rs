//! The cost of the hub messages on a frame's path: a head, its ends, its body, and a
//! credit, and of building a frame from them with `Frames`; and of the head, the
//! frames, and the replies of a program's write.

use std::sync::Arc;

use divan::Bencher;
use types::{
    channel,
    frame::{
        self, Form, Path, Range,
        key_set::{Group, Interner, KeySet},
    },
    sample::{Scalar, Type},
};
use wire::hub::{
    Credit, Due, Error, Frames, FromHome, FromReader, Head, Home, Mode, Open, Reader,
    Reply, Step, ends, keys,
    write::{self, Outcome, Refusal, Time},
};

const SERIES: [u32; 4] = [1, 3, 1_000, 100_000];

fn main() {
    divan::main();
}

fn run_of(series: u32) -> Vec<u8> {
    let mut run =
        vec![0; usize::try_from(series).expect("a u32 fits a usize") * ends::LEN];
    ends::encode((0..series).map(|place| (place, (place + 1) * 8)), &mut run);
    run
}

/// A head of no samples, so one reader takes it again on each iteration.
fn head(series: u32) -> Reply {
    Reply::Head(Head {
        path: Path::Live,
        range: Range { seq: 7, count: 0 },
        series,
    })
}

/// [`Reader::decode`], called out of line as production calls it from many sites, so
/// that no bench measures the inlining of its one call site.
#[inline(never)]
fn decode<'m>(reader: &mut Reader, message: &'m [u8]) -> Result<FromHome<'m>, Error> {
    reader.decode(Some(message))
}

/// A reader of `places` places that the home opened.
fn opened(places: u32) -> Reader {
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: places,
    });
    decode(&mut reader, &[1, 0, 0, 0, 0, 0, 0, 0, 0]).expect("the session opens");
    reader
}

#[divan::bench(args = SERIES)]
fn encode_ends(bencher: Bencher<'_, '_>, series: u32) {
    let mut run =
        vec![0; usize::try_from(series).expect("a u32 fits a usize") * ends::LEN];
    bencher.bench_local(|| {
        let ends = (0..series).map(|place| (place, (place + 1) * 8));
        ends::encode(divan::black_box(ends), &mut run);
    });
}

/// A home's list of each place and its home entry, sorted by place, with entries in
/// the reverse order of places, and the series length of each entry.
fn list_of(series: u32) -> (Vec<(u32, usize)>, Vec<usize>) {
    let places = (0..series)
        .map(|place| (place, usize::try_from(series - 1 - place).expect("fits")))
        .collect();
    (
        places,
        (0..series)
            .map(|entry| usize::try_from(entry % 13 + 1).expect("fits"))
            .collect(),
    )
}

/// The ends of a frame as the home writes them: from its list by place through
/// `frame::ends`, in messages of 184 ends.
#[divan::bench(args = SERIES)]
fn encode_ends_by_place(bencher: Bencher<'_, '_>, series: u32) {
    let (places, lens) = list_of(series);
    let mut run =
        vec![0; usize::try_from(series).expect("a u32 fits a usize") * ends::LEN];
    bencher.bench_local(|| {
        let lens = divan::black_box(&places)
            .iter()
            .map(|&(place, entry)| (place, lens[entry]));
        let mut each = frame::ends(lens)
            .map(|(place, end)| (place, u32::try_from(end).expect("fits")));
        for message in run.chunks_mut(184 * ends::LEN) {
            ends::encode(each.by_ref(), message);
        }
    });
}

/// Decodes the run of ends of a head, in one message.
#[divan::bench(args = SERIES)]
fn decode_ends(bencher: Bencher<'_, '_>, series: u32) {
    let run = run_of(series);
    let mut out = [0; 18];
    head(series).encode(&mut out);
    bencher
        .with_inputs(|| {
            let mut reader = opened(series);
            decode(&mut reader, &out).expect("the head decodes");
            reader
        })
        .bench_local_refs(|reader| match decode(reader, divan::black_box(&run)) {
            Ok(FromHome::Ends { ends, .. }) => sum(ends),
            other => panic!("the ends did not decode: {other:?}"),
        });
}

/// Decodes a frame on one reader: a head, its ends, and its body, one message each.
#[divan::bench(args = SERIES)]
fn decode_a_frame(bencher: Bencher<'_, '_>, series: u32) {
    let run = run_of(series);
    let body = vec![7; usize::try_from(series * 8).expect("a u32 fits a usize")];
    let mut out = [0; 18];
    head(series).encode(&mut out);
    let mut reader = opened(series);
    let mut frame = || {
        decode(&mut reader, divan::black_box(&out)).expect("the head decodes");
        let sum = match decode(&mut reader, divan::black_box(&run)) {
            Ok(FromHome::Ends { ends, .. }) => sum(ends),
            other => panic!("the ends did not decode: {other:?}"),
        };
        match decode(&mut reader, divan::black_box(&body)) {
            Ok(FromHome::Body { bytes, last: true }) => {
                sum.wrapping_add(u64::from(bytes[0]))
            }
            other => panic!("the body did not decode: {other:?}"),
        }
    };
    // A test runs the bench once, so this shows that one reader takes a frame again.
    frame();
    bencher.bench_local(frame);
}

/// Decodes a body of `messages` messages of 1 024 bytes, with where each starts.
#[divan::bench(args = [1, 8, 64])]
fn decode_a_body(bencher: Bencher<'_, '_>, messages: u32) {
    let mut out = [0; 18];
    head(1).encode(&mut out);
    let mut run = [0; ends::LEN];
    ends::encode([(0, messages * 1_024)], &mut run);
    let body = vec![7; usize::try_from(messages * 1_024).expect("a u32 fits a usize")];
    let mut reader = opened(1);
    let mut frame = || {
        decode(&mut reader, &out).expect("the head decodes");
        decode(&mut reader, &run).expect("the ends decode");
        body.chunks(1_024).fold(0, |sum: usize, message| {
            let at = reader.body().expect("the body comes next");
            match decode(&mut reader, divan::black_box(message)) {
                Ok(FromHome::Body { bytes, .. }) => sum.wrapping_add(at + bytes.len()),
                other => panic!("the body did not decode: {other:?}"),
            }
        })
    };
    frame();
    bencher.bench_local(frame);
}

/// An index, then `places - 1` channels of `I64`.
fn set_of(places: u32) -> Arc<KeySet> {
    let data: Vec<_> = (2..=u128::from(places))
        .map(|key| (channel::Key::from_u128(key), Type::Scalar(Scalar::I64)))
        .collect();
    Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &data,
    }])
}

/// Frames of `places` places that the home opened, with a pool for a frame of a
/// `body` byte body.
fn frames_of(places: u32, body: u32) -> (Frames, block::Pool) {
    let open = Open {
        mode: Mode::Latest,
        channels: places,
    };
    let opened = [1, 0, 0, 0, 0, 0, 0, 0, 0];
    let frames = Frames::open(Reader::new(&open), set_of(places), Some(&opened))
        .expect("opened");
    let bytes = (u64::from(body) + (1 << 20)).next_power_of_two();
    let config = block::Config::new(bytes).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    (frames, block::Pool::new(config, memory))
}

/// Builds one frame with the loop of `hub`'s remote reader: ask [`Frames::due`], then
/// give it the next of `messages`, a body message received in place, or a draft from
/// `pool`. Gives the frame's first body byte.
fn build<'m>(
    frames: &mut Frames,
    pool: &block::Pool,
    messages: impl IntoIterator<Item = &'m [u8]>,
) -> u8 {
    let mut messages = messages.into_iter();
    loop {
        let step = match frames.due().expect("the layout holds") {
            Due::Message => {
                let message = messages.next().expect("a message");
                frames.decode(Some(divan::black_box(message)))
            }
            Due::Body(rest) => {
                let message = divan::black_box(messages.next().expect("a message"));
                rest[..message.len()].copy_from_slice(message);
                frames.received(message.len())
            }
            Due::Draft(layout) => {
                let draft = layout.draft(pool, Form::Encoded).expect("a block");
                Ok(frames.draft(draft))
            }
        };
        match step.expect("the frame decodes") {
            Step::Head(_) | Step::More => {}
            Step::Frame(frame) => return frame.body()[0],
        }
    }
}

/// Builds a frame of `series` series of 8 bytes with [`Frames`], the body in one
/// message.
#[divan::bench(args = SERIES)]
fn build_a_frame(bencher: Bencher<'_, '_>, series: u32) {
    let (mut frames, pool) = frames_of(series, series * 8);
    let mut head = [0; 18];
    self::head(series).encode(&mut head);
    let run = run_of(series);
    let body = vec![7; usize::try_from(series * 8).expect("a u32 fits a usize")];
    let mut frame = || build(&mut frames, &pool, [&head, &run[..], &body]);
    frame();
    bencher.bench_local(frame);
}

/// As [`build_a_frame`], but each head has one sample, one seq past the head before
/// it, so that each frame moves the live position of the latest session.
#[divan::bench(args = SERIES)]
fn build_a_sampled_frame(bencher: Bencher<'_, '_>, series: u32) {
    let (mut frames, pool) = frames_of(series, series * 8);
    let run = run_of(series);
    let body = vec![7; usize::try_from(series * 8).expect("a u32 fits a usize")];
    let mut seq = 0;
    let mut frame = || {
        let mut head = [0; 18];
        Reply::Head(Head {
            path: Path::Live,
            range: Range { seq, count: 1 },
            series,
        })
        .encode(&mut head);
        seq += 1;
        build(&mut frames, &pool, [&head, &run[..], &body])
    };
    frame();
    bencher.bench_local(frame);
}

/// Builds a frame of one series with [`Frames`], its body in `messages` messages of
/// 1 024 bytes received in place.
#[divan::bench(args = [1, 8, 64])]
fn build_a_body(bencher: Bencher<'_, '_>, messages: u32) {
    let (mut frames, pool) = frames_of(1, messages * 1_024);
    let mut head = [0; 18];
    self::head(1).encode(&mut head);
    let mut run = [0; ends::LEN];
    ends::encode([(0, messages * 1_024)], &mut run);
    let body = vec![7; usize::try_from(messages * 1_024).expect("a u32 fits a usize")];
    let mut frame = || {
        let messages = [&head, &run[..]].into_iter().chain(body.chunks(1_024));
        build(&mut frames, &pool, messages)
    };
    frame();
    bencher.bench_local(frame);
}

#[divan::bench]
fn encode_and_decode_a_head(bencher: Bencher<'_, '_>) {
    let mut out = [0; 18];
    bencher
        .with_inputs(|| opened(3))
        .bench_local_refs(|reader| {
            divan::black_box(head(3)).encode(&mut out);
            match decode(reader, divan::black_box(&out)) {
                Ok(FromHome::Head(head)) => head,
                other => panic!("the head did not decode: {other:?}"),
            }
        });
}

/// Decodes a credit on one home, after the open and its keys.
#[divan::bench]
fn decode_a_credit(bencher: Bencher<'_, '_>) {
    let mut open = [0; 13];
    Open {
        mode: Mode::Complete { limit_bytes: 0 },
        channels: 1,
    }
    .encode(&mut open);
    let mut key = [0; keys::LEN];
    keys::encode(&[channel::Key::from_u128(1)], &mut key);
    let mut credit = [0; Credit::LEN];
    Credit { limit_bytes: 64 }.encode(&mut credit);
    let mut home = Home::default();
    for message in [open.as_slice(), &key] {
        home.decode(message).expect("the open decodes");
    }
    bencher.bench_local(|| match home.decode(divan::black_box(&credit)) {
        Ok(FromReader::Credit(credit)) => credit,
        other => panic!("the credit did not decode: {other:?}"),
    });
}

/// Encodes the head of a program's frame of 1,000 samples.
#[divan::bench]
fn encode_a_write_head(bencher: Bencher<'_, '_>) {
    let head = write::Head {
        time: Time::Frame(Path::Live),
        form: Form::Raw,
        count: 1_000,
        series: 3,
    };
    let mut out = [0; write::Head::LEN];
    bencher.bench_local(|| divan::black_box(head).encode(&mut out));
}

/// Decodes a program's frame of `series` series of one 8 byte sample at the node with
/// [`write::Frames`]: its head, a run of one message, a draft from a pool, and a body
/// of one message received in place.
#[divan::bench(args = SERIES)]
fn decode_a_write_frame(bencher: Bencher<'_, '_>, series: u32) {
    decode_write(bencher, series, true);
}

/// [`decode_a_write_frame`], with the body message copied by [`write::Frames::decode`].
#[divan::bench(args = SERIES)]
fn decode_a_copied_write_frame(bencher: Bencher<'_, '_>, series: u32) {
    decode_write(bencher, series, false);
}

/// Decodes write frames as [`decode_a_write_frame`] says, with the body received
/// `in_place` or copied. Each frame gives its first body byte.
fn decode_write(bencher: Bencher<'_, '_>, series: u32, in_place: bool) {
    let channels = (1..=u128::from(series)).map(|key| {
        let data = if key == 1 { Scalar::Stamp } else { Scalar::I64 };
        (channel::Key::from_u128(key), Type::Scalar(data), 0)
    });
    let set = KeySet::ordered(channels).expect("no key repeats");
    let mut frames = write::Frames::new(Arc::new(set));
    let bytes = (u64::from(series) * 8 + (1 << 20)).next_power_of_two();
    let config = block::Config::new(bytes).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    let pool = block::Pool::new(config, memory);
    let mut head = [0; write::Head::LEN];
    write::Head {
        time: Time::Frame(Path::Live),
        form: Form::Raw,
        count: 1,
        series,
    }
    .encode(&mut head);
    let run = run_of(series);
    let body = vec![7; usize::try_from(series * 8).expect("a u32 fits a usize")];
    let mut frame = || {
        let mut messages = [&head[..], &run[..], &body[..]].into_iter();
        loop {
            let step = match frames.due().expect("the layout holds") {
                write::Due::Body(rest) if in_place => {
                    let message = divan::black_box(messages.next().expect("a message"));
                    rest[..message.len()].copy_from_slice(message);
                    frames.received(message.len())
                }
                write::Due::Message | write::Due::Body(_) => {
                    let message = messages.next().expect("a message");
                    frames.decode(divan::black_box(message))
                }
                write::Due::Draft { layout, form } => {
                    let draft = layout.draft(&pool, form).expect("a block");
                    Ok(frames.draft(draft))
                }
            };
            match step.expect("the frame decodes") {
                write::Step::Head(_) | write::Step::More => {}
                write::Step::Frame { mut draft, .. } => return draft.body_mut()[0],
            }
        }
    };
    frame();
    bencher.bench_local(frame);
}

/// The replies of a write: a frame taken, a frame refused, and the seqs on disk.
fn write_replies() -> [write::Reply; 3] {
    [
        write::Reply::Outcome {
            frame: 5,
            outcome: Outcome::Taken {
                range: Range {
                    seq: 7,
                    count: 1_000,
                },
            },
        },
        write::Reply::Outcome {
            frame: 6,
            outcome: Outcome::Refused(Refusal::Full),
        },
        write::Reply::Stored { frames: 1_006 },
    ]
}

/// Encodes each reply of [`write_replies`].
#[divan::bench]
fn encode_write_replies(bencher: Bencher<'_, '_>) {
    let replies = write_replies();
    let mut out = [0; Outcome::LEN_MAX];
    bencher.bench_local(|| {
        for reply in divan::black_box(&replies) {
            reply.encode(&mut out[..reply.encoded_len()]);
        }
    });
}

/// Decodes each reply of [`write_replies`].
#[divan::bench]
fn decode_write_replies(bencher: Bencher<'_, '_>) {
    let messages = write_replies().map(|reply| {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        out
    });
    bencher.bench_local(|| {
        for message in divan::black_box(&messages) {
            divan::black_box(write::Reply::decode(message).expect("the reply decodes"));
        }
    });
}

fn sum(ends: ends::Iter<'_>) -> u64 {
    ends.fold(0, |sum, (place, end)| {
        sum.wrapping_add(u64::from(place))
            .wrapping_add(u64::from(end))
    })
}
