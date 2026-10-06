//! The cost of one read of the boot clock against std's monotonic clock.

use std::time::Instant;

use divan::Bencher;

const READS: u64 = 1_000_000;

fn main() {
    divan::main();
}

/// One read of `os::clock`.
#[divan::bench(sample_count = 20)]
fn now(bencher: Bencher<'_, '_>) {
    let clock = os::clock();
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench_local(|| {
            for _ in 0..READS {
                divan::black_box(clock.now());
            }
        });
}

/// One read of std's monotonic clock, which stops in a suspend, for comparison.
#[divan::bench(sample_count = 20)]
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn instant_now(bencher: Bencher<'_, '_>) {
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench_local(|| {
            for _ in 0..READS {
                divan::black_box(Instant::now());
            }
        });
}
