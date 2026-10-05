//! The cost of checking one frame's stamps and giving its samples their seq, which the
//! home does for every frame it accepts.

use divan::Bencher;
use types::time::Stamp;

fn main() {
    divan::main();
}

/// A live frame of `samples` stamps.
#[divan::bench(args = [1, 1024])]
fn accept(bencher: Bencher<'_, '_>, samples: i64) {
    let stamps: Vec<Stamp> = (1..=samples).map(Stamp::from_nanos).collect();
    let now = Stamp::from_nanos(samples);
    bencher.bench_local(|| home::bench::accept(divan::black_box(&stamps), now));
}
