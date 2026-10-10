//! The cost of the hub messages on a frame's path: a head, its ends, its body, and a
//! credit.

use divan::Bencher;
use types::{
    channel,
    frame::{self, Path, Range},
};
use wire::hub::{
    Credit, Error, FromHome, FromReader, Head, Home, Mode, Open, Reader, Reply, ends,
    keys,
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
    reader.decode(message)
}

/// A reader of `places` places that the home opened.
fn opened(places: u32) -> Reader {
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: places,
    });
    decode(&mut reader, &[1]).expect("the session opens");
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

fn sum(ends: ends::Iter<'_>) -> u64 {
    ends.fold(0, |sum, (place, end)| {
        sum.wrapping_add(u64::from(place))
            .wrapping_add(u64::from(end))
    })
}
