//! The per-record cost of the ring: one append, one CRC, one lap of recovery. Divan
//! prints allocation rows only when a benchmark allocates, so none must appear.

use buffer::bench::{Ring, crc};
use divan::counter::BytesCount;
use divan::{AllocProfiler, Bencher};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const AREA: u64 = 1 << 26;
const BODY_MAX: usize = 1 << 20;
const BODIES: [usize; 3] = [64, 4087, 1 << 20];

fn main() {
    divan::main();
}

/// One record planned and freed at once, so the ring never fills.
#[divan::bench(args = BODIES)]
fn append(bencher: Bencher<'_, '_>, len: usize) {
    let body = vec![0xA5; len];
    let mut ring = Ring::new(AREA, BODY_MAX);
    bencher
        .counter(BytesCount::new(len))
        .bench_local(|| ring.plan(divan::black_box(&body)));
}

#[divan::bench(args = BODIES)]
fn crc32c(bencher: Bencher<'_, '_>, len: usize) {
    let bytes = vec![0xA5; len];
    bencher
        .counter(BytesCount::new(len))
        .bench_local(|| crc(divan::black_box(&bytes)));
}

/// Recovery over a full area of records of one size.
#[divan::bench(args = BODIES)]
fn walk(bencher: Bencher<'_, '_>, len: usize) {
    let body = vec![0xA5; len];
    let mut ring = Ring::new(AREA, BODY_MAX);
    while ring.write(&body) {}
    let records = ring.records();
    bencher
        .counter(BytesCount::new(
            records * (u64::try_from(len).expect("a length") + 9),
        ))
        .bench_local(|| {
            let found = divan::black_box(&ring).walk();
            assert_eq!(found, records, "the walk finds every record");
        });
}
