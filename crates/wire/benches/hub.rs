//! The cost of the hub messages on a frame's path: a head and its ends.

use divan::Bencher;
use types::frame::{self, Path, Range};
use wire::hub::{Head, Reply, ends};

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
            let count = message.len() / ends::LEN;
            ends::encode(each.by_ref().take(count), message);
        }
    });
}

#[divan::bench(args = SERIES)]
fn decode_ends(bencher: Bencher<'_, '_>, series: u32) {
    let run = run_of(series);
    bencher.bench_local(|| {
        ends::decode(divan::black_box(&run))
            .expect("the run has ends")
            .fold(0_u64, |sum, (place, end)| {
                sum.wrapping_add(u64::from(place))
                    .wrapping_add(u64::from(end))
            })
    });
}

#[divan::bench]
fn encode_and_decode_a_head(bencher: Bencher<'_, '_>) {
    let head = Reply::Head(Head {
        path: Path::Live,
        range: Range { seq: 7, count: 1 },
        series: 3,
    });
    let mut out = [0; 18];
    bencher.bench_local(|| {
        divan::black_box(head).encode(&mut out);
        Reply::decode(divan::black_box(&out))
    });
}
