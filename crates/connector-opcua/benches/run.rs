//! The time of one run of the open62541 event loop of a client when 1, 100, or 10,000
//! repeated timers of 1 ms are due, and the time to make and drop a client, its loop,
//! and 100 timers, each C allocation of which goes through the global allocator. The
//! client's own timer has an interval of 1 s, so it is due in at most one sample.

use connector_opcua::bench::Client;
use divan::Bencher;
use divan::counter::ItemsCount;
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

/// The clients of a sample. A run with one due timer is near the timer precision of
/// divan, so a sample runs each client once, and `ItemsCount` gives the runs.
const CLIENTS: usize = 16;

/// One input for each sample: a batch of inputs would move the clock before the first
/// run, so the later runs of the batch would find no timer due.
#[divan::bench(args = [1, 100, 10_000], sample_size = 1, sample_count = 1000)]
fn run(bencher: Bencher<'_, '_>, timers: usize) {
    let (mut sim, clock) = sim();
    let mut clients: Vec<_> = (0..CLIENTS)
        .map(|_| Client::new(env::clock::Clock::clone(&clock), timers))
        .collect();
    bencher
        .counter(ItemsCount::new(CLIENTS))
        .with_inputs(|| sim.run_for(Span::MILLISECOND).expect("the run has no task"))
        .bench_local_values(|()| clients.iter_mut().for_each(Client::run));
}

#[divan::bench]
fn new(bencher: Bencher<'_, '_>) {
    let (_sim, clock) = sim();
    bencher.bench_local(|| drop(Client::new(env::clock::Clock::clone(&clock), 100)));
}
