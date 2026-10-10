//! One run of the open62541 event loop with due timers allocates nothing, in C or in
//! Rust, and the fuzz round trip makes a fixed count of allocations, and none after a
//! decode that fails. A read through the connection manager makes a fixed count of
//! allocations, and a drive with nothing ready makes none. The count covers each
//! thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use connector_opcua::bench::{Client, Manager};
use sim::Sim;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    the_count_holds_the_allocations_of_c();
    a_run_allocates_nothing();
    the_drop_frees_the_client();
    the_fuzz_round_trip_allocates_for_each_step();
    a_failed_decode_stops_the_round_trip();
    a_read_through_the_manager_allocates_a_fixed_count();
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
    assert_eq!(count, 10, "the round trip of a Variant");
}

/// A `Boolean` (0) with no byte fails before C allocates, so only the memory of the
/// value is allocated.
fn a_failed_decode_stops_the_round_trip() {
    let ((), count) = ALLOCATOR.count(|| connector_opcua::fuzz::decode(&[0, 0]));
    assert_eq!(count, 1, "a failed decode");
    // A `Variant` of a `Boolean` with no byte: C allocates the `Boolean` first.
    let ((), count) = ALLOCATOR.count(|| connector_opcua::fuzz::decode(&[23, 0, 0x01]));
    assert_eq!(count, 2, "a failed decode after C allocates");
}

/// The delay of the default link.
fn delay() -> Span {
    sim::link::Config::default().delay
}

/// The allocations of each drive of reads 2 to 100 with `idle` idle clients: the ask,
/// the answer of the server, the read of the answer, and a drive with nothing ready.
fn reads(idle: usize) -> Vec<[u64; 4]> {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let run = move |node: sim::node::Node, _| async move {
        let (clock, address) = (node.clock(), node.addresses()[0]);
        let body = async |manager: &Manager| {
            let mut counts = Vec::new();
            for read in 1..=100 {
                let ((), ask) = ALLOCATOR.count(|| manager.ask());
                clock.sleep(delay()).await;
                let ((), answer) = ALLOCATOR.count(|| manager.drive());
                clock.sleep(delay()).await;
                let ((), receive) = ALLOCATOR.count(|| manager.drive());
                let ((), pass) = ALLOCATOR.count(|| manager.drive());
                assert_eq!(manager.answers(), read, "{idle} idle");
                if read > 1 {
                    counts.push([ask, answer, receive, pass]);
                }
            }
            counts
        };
        Manager::scope(node.clock(), node.net(), address, idle, body).await
    };
    sim.run_on(&node, run).expect("the run ends")
}

/// open62541 allocates for each message it sends and decodes, and the sim net
/// allocates a segment for each write, one in the ask and one in the answer. So a read
/// is not free. The count is the same for each read and each count of idle
/// connections, so a Rust allocation on the path of a message adds one to a count.
fn a_read_through_the_manager_allocates_a_fixed_count() {
    for idle in [0, 15] {
        for (at, counts) in reads(idle).into_iter().enumerate() {
            let read = at + 2;
            assert_eq!(counts, [4, 5, 2, 0], "{idle} idle, read {read}");
        }
    }
}
