//! One run of the open62541 event loop with due timers allocates nothing, in C or in
//! Rust, and the fuzz round trip makes a fixed count of allocations, and none after a
//! decode that fails. A read through the connection manager makes a fixed count of
//! allocations, and a drive with nothing ready makes none. A tick of the `opcua` kind
//! allocates only what its read allocates, and the write of its frame allocates
//! nothing for a channel of any width. The count covers each thread, so this
//! binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::{Future as _, poll_fn};
use std::pin::pin;

use connector_opcua::bench::{Client, Manager, client};
#[path = "common/kind.rs"]
#[expect(dead_code, reason = "this binary measures only the ticks")]
mod kind;
use env::rng::Rng;
use sim::Sim;
use types::sample::Scalar;
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
    a_read_of_a_client_allocates_a_fixed_count();
    a_tick_of_the_kind_allocates_what_its_read_allocates();
    a_write_of_a_frame_of_any_type_allocates_nothing();
}

fn sim() -> (Sim, env::clock::Clock, Rng) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    (sim, node.clock(), node.entropy().rng())
}

/// The positive control: the copy allocates one entry for each timer. The two loops
/// live at once on this thread, so they take one `Rng`.
fn the_count_holds_the_allocations_of_c() {
    let ((_sim, none, mut rng), (_other, timers, _)) = (sim(), sim());
    let (none, base) = ALLOCATOR.count(|| Client::new(none, &mut rng, 0));
    let (timers, count) = ALLOCATOR.count(|| Client::new(timers, &mut rng, 100));
    assert_eq!(count - base, 100, "the timers of 100 against 0 allocated");
    drop((none, timers));
}

/// Each run for 2 s of simulated time, so the housekeeping timer of the client runs
/// too. After each run, the timers wait 1 ms more.
fn a_run_allocates_nothing() {
    for timers in [1, 100, 10_000] {
        let (mut sim, clock, mut rng) = sim();
        let mut client =
            Client::new(env::clock::Clock::clone(&clock), &mut rng, timers);
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
    let (_sim, clock, mut rng) = sim();
    let client = Client::new(clock, &mut rng, 0);
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
        let rng = &mut node.entropy().rng();
        Manager::scope(node.clock(), node.net(), address, idle, rng, body).await
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
            assert_eq!(counts, [3, 5, 2, 0], "{idle} idle, read {read}");
        }
    }
}

/// The allocations in the polls of reads 2 to 100 of `count` copies of `variable` by
/// one client. The server runs on its own manager, outside the polls.
fn client_reads(variable: client::Variable, count: usize) -> Vec<u64> {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let run = move |node: sim::node::Node, _| async move {
        let address = node.addresses()[0];
        let body = async |server: &client::Server| {
            let mut client = server.connect(count).await;
            let mut counts = Vec::new();
            for _ in 1..=100 {
                let mut read = pin!(client.read(variable));
                let mut count = 0;
                poll_fn(|cx| {
                    let (poll, allocations) =
                        ALLOCATOR.count(|| read.as_mut().poll(cx));
                    count += allocations;
                    poll
                })
                .await;
                counts.push(count);
            }
            client.close().await;
            counts.split_off(1)
        };
        let rng = node.entropy().rng();
        client::Server::scope(node.clock(), node.net(), address, rng, body).await
    };
    sim.run_on(&node, run).expect("the run ends")
}

/// As through the manager, the ask allocates 3 and the answer allocates 1 for its
/// results array and 1 for the value of each node. The sleep of a drive until the next
/// timer of the loop allocates nothing, since the manager keeps it. The client swaps
/// two buffers of values. The connect fills one with 1 value, and read 2 is the first
/// to fill it with more. The sim net allocates one more for each segment of 1460 bytes
/// past the first: the ask of 64 names takes 2. A read of 300 or 600 nodes takes 2 or 3
/// requests, of [`client::PER_READ`] nodes and the rest. Each request allocates 4 for
/// its ask and its results array, and each node 1: 308 and 612. The rest, 2 and 7, is
/// the segments of the asks, as measured: the sim net cuts the bytes that the client
/// writes into segments as the window of the server lets them go, so the asks of one
/// read share segments.
fn a_read_of_a_client_allocates_a_fixed_count() {
    let cases = [
        (client::Variable::Number, 1, 5),
        (client::Variable::Name, 1, 5),
        (client::Variable::Number, 64, 68),
        (client::Variable::Name, 64, 69),
        (client::Variable::Number, 300, 310),
        (client::Variable::Number, 600, 619),
    ];
    for (variable, count, each) in cases {
        let mut counts = vec![each + u64::from(count > 1)];
        counts.extend([each; 98]);
        assert_eq!(
            client_reads(variable, count),
            counts,
            "reads 2 to 100 of {count} of {variable:?}"
        );
    }
}

/// Ticks 3 to 100 of a run of the `opcua` kind allocate what a read of a client
/// allocates for its count of nodes, and nothing more: the frame and its write
/// allocate nothing, also when values of another type are left out and named in the
/// note. Tick 1 connects, and tick 2 fills the second buffer of values.
fn a_tick_of_the_kind_allocates_what_its_read_allocates() {
    let runs: [(usize, &[Scalar], u64); 4] = [
        (1, &[Scalar::F64], 5),
        (64, &[Scalar::F64], 68),
        (1, &[Scalar::F32], 5),
        (64, &[Scalar::F64, Scalar::F32], 68),
    ];
    for (nodes, scalars, read) in runs {
        let count = |poll: &mut dyn FnMut()| ALLOCATOR.count(poll).1;
        let ticks = kind::run(nodes, scalars, 100, count).ticks;
        let of = format!("ticks 3 to 100 of {nodes} nodes of {scalars:?}");
        assert_eq!(ticks[2..], [read; 98], "{of}");
    }
}

/// Writes 21 to 100 of a frame of the `opcua` kind allocate nothing, for channels of
/// each width, with one value or each value left out, and with values left out that
/// start after write 20 and change in count. The test home grows the `Vec`s of its
/// groups in its first two commits, which the first 20 writes cover, as in the `kind`
/// bench.
fn a_write_of_a_frame_of_any_type_allocates_nothing() {
    /// The count of Bad values of each write.
    type Bad = fn(usize) -> usize;
    let runs: [(&[Scalar], Bad, &str); 7] = [
        (&[Scalar::Bool], |_| 0, "none"),
        (&[Scalar::I16], |_| 0, "none"),
        (&[Scalar::F32], |_| 0, "none"),
        (&[Scalar::F64], |_| 0, "none"),
        (&[Scalar::F64], |_| 1, "1"),
        (&[Scalar::F64], |_| 64, "64"),
        (
            &[Scalar::F64],
            |at| [0, 63, 1, 32][at.saturating_sub(16) / 8 % 4],
            "late",
        ),
    ];
    for (scalars, bad, bads) in runs {
        let count = |work: &mut dyn FnMut()| ALLOCATOR.count(work).1;
        let writes = kind::writes(64, scalars, 100, bad, count, false).0;
        assert_eq!(
            writes[20..],
            [0; 80],
            "writes 21 to 100 of {scalars:?}, Bad values: {bads}"
        );
    }
}
