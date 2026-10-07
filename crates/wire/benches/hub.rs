//! The cost of the hub messages on a frame's path: a head and its ends.

use divan::Bencher;
use types::frame::{Path, Range};
use wire::hub::{FromHome, Head, Mode, Open, Reader, Reply, ends};

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

fn head(series: u32) -> Reply {
    Reply::Head(Head {
        path: Path::Live,
        range: Range { seq: 7, count: 1 },
        series,
    })
}

/// A reader of `places` places that the home opened.
fn opened(places: u32) -> Reader {
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: places,
    });
    reader.decode(&[1]).expect("the session opens");
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

/// Decodes a head and the run of its ends, in one message.
#[divan::bench(args = SERIES)]
fn decode_ends(bencher: Bencher<'_, '_>, series: u32) {
    let run = run_of(series);
    let mut out = [0; 18];
    head(series).encode(&mut out);
    bencher
        .with_inputs(|| opened(series))
        .bench_local_refs(|reader| {
            reader
                .decode(divan::black_box(&out))
                .expect("the head decodes");
            match reader.decode(divan::black_box(&run)) {
                Ok(FromHome::Ends { ends, .. }) => {
                    ends.fold(0_u64, |sum, (place, end)| {
                        sum.wrapping_add(u64::from(place))
                            .wrapping_add(u64::from(end))
                    })
                }
                other => panic!("the ends did not decode: {other:?}"),
            }
        });
}

#[divan::bench]
fn encode_and_decode_a_head(bencher: Bencher<'_, '_>) {
    let mut out = [0; 18];
    bencher
        .with_inputs(|| opened(3))
        .bench_local_refs(|reader| {
            divan::black_box(head(3)).encode(&mut out);
            match reader.decode(divan::black_box(&out)) {
                Ok(FromHome::Head(head)) => head,
                other => panic!("the head did not decode: {other:?}"),
            }
        });
}
