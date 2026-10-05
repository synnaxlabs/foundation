//! The per-frame cost of the gate: a write from the holder that the home accepts, and
//! the read of the handoff, with none waiting and with one waiting.

use control::{Gate, Lease, Writer};
use divan::Bencher;
use types::authority::Authority;
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
        let permit = gate
            .check(divan::black_box(key), Monotonic(now))
            .expect("the holder writes");
        gate.renew(permit);
    });
}

#[divan::bench(args = [false, true])]
fn handoff(bencher: Bencher<'_, '_>, waiting: bool) {
    let mut gate = Gate::new();
    gate.open(writer("plc.valve"), None, Monotonic(0));
    gate.open(writer("plc.backup"), None, Monotonic(0));
    if !waiting {
        gate.recorded();
    }
    let gate = &gate;
    bencher.bench_local(move || divan::black_box(gate).handoff());
}
