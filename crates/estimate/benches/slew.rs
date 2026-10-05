//! The per-reading cost of mesh time from a slew, on a frame's path.

use divan::Bencher;
use estimate::{Drift, Measurement, Slew};
use types::time::{Monotonic, Span};

fn main() {
    divan::main();
}

#[divan::bench]
fn at_mid_slew(bencher: Bencher<'_, '_>) {
    let estimate = |offset, error| Measurement::new(Monotonic(0), offset, error);
    let first = estimate(Span::ZERO, Span::SECOND).expect("valid");
    let target = estimate(Span::SECOND, Span::MICROSECOND).expect("valid");
    let slew = Slew::new(first).toward(Monotonic(0), Drift::UNDISCIPLINED, target);
    let mut now = 1_000_000_000;
    bencher.bench_local(|| {
        now += 1_000;
        let drift = divan::black_box(Drift::UNDISCIPLINED);
        divan::black_box(slew).at(Monotonic(now), drift)
    });
}
