//! One run of the open62541 event loop with due timers allocates nothing, in C or in
//! Rust, and the fuzz round trip makes a fixed count of allocations. The count covers
//! each thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use connector_opcua::bench::Client;
use sim::Sim;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    the_count_holds_the_allocations_of_c();
    a_run_allocates_nothing();
    the_drop_frees_the_client();
    the_fuzz_round_trip_allocates_for_each_step();
}

fn sim() -> (Sim, env::clock::Clock) {
    let mut sim = Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    (sim, clock)
}

/// The positive control: the copy allocates one entry for each timer.
fn the_count_holds_the_allocations_of_c() {
    let ((_sim, none), (_other, timers)) = (sim(), sim());
    let (none, base) = ALLOCATOR.count(|| Client::new(none, 0));
    let (timers, count) = ALLOCATOR.count(|| Client::new(timers, 100));
    assert_eq!(count - base, 100, "the timers of 100 against 0 allocated");
    drop((none, timers));
}

/// Each run for 2 s of simulated time, so the housekeeping timer of the client runs
/// too. After each run, the timers wait 1 ms more.
fn a_run_allocates_nothing() {
    for timers in [1, 100, 10_000] {
        let (mut sim, clock) = sim();
        let mut client = Client::new(env::clock::Clock::clone(&clock), timers);
        for at in 1..=2000 {
            sim.run_for(Span::MILLISECOND).expect("the run has no task");
            let ((), count) = ALLOCATOR.count(|| client.run());
            assert_eq!(count, 0, "{timers} timers, at {at} ms");
            assert_eq!(
                client.next(),
                Some(clock.now() + Span::MILLISECOND),
                "the run ran the timers, at {at} ms"
            );
        }
    }
}

/// The client of the copy holds this URI when it has no other.
const URI: &[u8] = b"urn:open62541.unconfigured.application";

fn the_drop_frees_the_client() {
    let (_sim, clock) = sim();
    let client = Client::new(clock, 0);
    let ((), freed) = ALLOCATOR.freed_holding(URI, || drop(client));
    assert_eq!(freed, 1, "the drop freed the URI of the client");
}

/// A `Variant` (23, as `ffi` is private) of 7 `ExtensionObject` values, then the
/// zeros that #435 needs.
fn the_fuzz_round_trip_allocates_for_each_step() {
    let mut data = vec![23, 0, 0x96, 7, 0, 0, 0];
    data.resize(data.len() + 7 * 4, 0);
    let ((), count) = ALLOCATOR.count(|| connector_opcua::fuzz::decode(&data));
    assert_eq!(count, 11, "the round trip of a Variant");
}
