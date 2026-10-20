//! The cost of the changes of one Publish answer of `client::Client`, against the test
//! server of open62541 over the sim net. Run with `cargo bench -p connector-opcua
//! --bench changes`.
//!
//! - `ns N`: the ns in the polls of `changes` that give an answer of N changes: the
//!   decode of the answer, the copy of its changes, and the Publish request that it
//!   sends again.
//! - `allocs N`: the allocations in those polls, which the count of changes does not
//!   change.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::{Future as _, poll_fn};
use std::pin::pin;
use std::time::{Duration, Instant};

use connector_opcua::bench::client::{Client, Server};
use sim::Sim;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const WARMUP: usize = 10;
const ROUNDS: usize = 1000;

/// The changes in each Publish answer.
const COUNTS: [usize; 3] = [1, 100, 1000];

fn main() {
    let mut lines = Vec::new();
    for count in COUNTS {
        let [nanos, allocations] = answers(count);
        lines.push((format!("ns {count}"), nanos));
        lines.push((format!("allocs {count}"), allocations));
    }
    print(&mut lines);
}

/// The ns and the allocations of the polls of each timed answer of `count` changes.
fn answers(count: usize) -> [Vec<u64>; 2] {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let run = move |node: sim::node::Node, _| async move {
        let address = node.addresses()[0];
        let body = async |server: &Server| {
            let mut client = server.connect(1).await;
            client.subscribe(count).await;
            let out = changes(&mut client, count).await;
            client.close().await;
            out
        };
        let rng = node.entropy().rng();
        Server::counting(node.clock(), node.net(), address, rng, count, body).await
    };
    sim.run_on(&node, run).expect("the run ends")
}

/// The ns and the allocations of the polls of each timed answer of `count` changes.
/// The first takes can give 2 answers at once, so only the warmup takes them.
async fn changes(client: &mut Client, count: usize) -> [Vec<u64>; 2] {
    let (mut nanos, mut allocations) = (Vec::new(), Vec::new());
    for answer in 0..WARMUP + ROUNDS {
        let mut future = pin!(client.changes());
        let (mut spent, mut allocated) = (Duration::ZERO, 0);
        let changes = poll_fn(|cx| {
            let start = now();
            let (poll, count) = ALLOCATOR.count(|| future.as_mut().poll(cx));
            spent += now().duration_since(start);
            allocated += count;
            poll
        })
        .await;
        if answer >= WARMUP {
            assert_eq!(changes, count, "the changes of answer {answer}");
            nanos.push(u64::try_from(spent.as_nanos()).expect("under 2^64 ns"));
            allocations.push(allocated);
        }
    }
    [nanos, allocations]
}

#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn now() -> Instant {
    Instant::now()
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &mut [(String, Vec<u64>)]) {
    println!("each Publish answer of N changes, over {ROUNDS} answers");
    println!("pN: the answer at percentile N");
    println!("{:<12} {:>9} {:>9} {:>9}", "line", "p10", "p50", "p90");
    for (name, values) in lines {
        values.sort_unstable();
        let at = |percent: usize| values[ROUNDS * percent / 100];
        println!("{name:<12} {:>9} {:>9} {:>9}", at(10), at(50), at(90));
    }
}
