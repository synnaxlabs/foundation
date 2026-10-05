//! The per-frame cost of the gate: one write from the holder.

use control::{Authority, Gate, Lease, Writer};
use divan::Bencher;
use types::time::{Monotonic, Span};

fn main() {
    divan::main();
}

fn writer(subject: &str) -> Writer {
    Writer {
        subject: subject.parse().expect("valid name"),
        authority: Authority(100),
    }
}

#[divan::bench(args = [false, true])]
fn write_by_holder(bencher: Bencher<'_, '_>, leased: bool) {
    let lease = leased.then(|| Lease::new(Span::SECOND).expect("positive lease"));
    let mut gate = Gate::new();
    let key = gate.open(writer("plc.valve"), lease, Monotonic(0));
    gate.open(writer("plc.backup"), None, Monotonic(0));
    let mut now = 0;
    bencher.bench_local(|| {
        now += 1;
        gate.write(divan::black_box(key), Monotonic(now))
    });
}
