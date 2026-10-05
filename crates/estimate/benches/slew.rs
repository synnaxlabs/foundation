//! The per-reading cost of mesh time from a slew, on a frame's path.

use divan::Bencher;
use estimate::{Drift, Measurement, Slew};
use types::time::{Monotonic, Span};

fn main() {
    divan::main();
}

#[divan::bench]
fn at_mid_slew(bencher: Bencher<'_, '_>) {
    let estimate = |offset| Measurement::new(Monotonic(0), offset, Span::MICROSECOND);
    let first = estimate(Span::ZERO).expect("valid");
    let target = estimate(Span::SECOND).expect("valid");
    let slew = Slew::new(first).toward(Monotonic(0), target);
    let mut now = 1_000_000_000;
    bencher.bench_local(|| {
        now += 1_000;
        let drift = divan::black_box(Drift::UNDISCIPLINED);
        divan::black_box(slew).at(Monotonic(now), drift)
    });
}
