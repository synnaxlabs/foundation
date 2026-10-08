//! The time of one run of the open62541 event loop when the housekeeping timer of a
//! client and 1, 100, or 10,000 repeated timers are due, and the time to make and drop
//! a client, its loop, and 100 timers, each C allocation of which goes through the
//! global allocator.

use connector_opcua::bench::Client;
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

/// One run for each sample: a batch of inputs would move the clock before the first
/// run, so the later runs of the batch would find no timer due.
#[divan::bench(args = [1, 100, 10_000], sample_size = 1, sample_count = 1000)]
fn run(bencher: Bencher<'_, '_>, timers: usize) {
    let (mut sim, clock) = sim();
    let mut client = Client::new(clock, timers);
    bencher
        .with_inputs(|| sim.run_for(Span::MILLISECOND).expect("the run has no task"))
        .bench_local_values(|()| client.run());
}

#[divan::bench]
fn new(bencher: Bencher<'_, '_>) {
    let (_sim, clock) = sim();
    bencher.bench_local(|| drop(Client::new(env::clock::Clock::clone(&clock), 100)));
}
