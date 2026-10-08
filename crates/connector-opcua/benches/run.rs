//! The time of one run of the open62541 event loop when the housekeeping timer of a
//! client and 1, 100, or 10,000 repeated timers are due, with the time to move the
//! simulated clock 1 ms, which each run also takes, as its own bench.

use connector_opcua::bench::Iteration;
use divan::Bencher;
use sim::Sim;
use types::time::Span;

fn main() {
    divan::main();
}

fn sim() -> (Sim, env::clock::Clock) {
    let mut sim = Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    (sim, clock)
}

#[divan::bench]
fn advance(bencher: Bencher<'_, '_>) {
    let (mut sim, _clock) = sim();
    bencher
        .bench_local(|| sim.run_for(Span::MILLISECOND).expect("the run has no task"));
}

#[divan::bench(args = [1, 100, 10_000])]
fn run(bencher: Bencher<'_, '_>, timers: usize) {
    let (mut sim, clock) = sim();
    let iteration = Iteration::new(clock, timers);
    bencher.bench_local(|| {
        sim.run_for(Span::MILLISECOND).expect("the run has no task");
        iteration.run();
    });
}
