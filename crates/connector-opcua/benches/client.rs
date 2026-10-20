//! The cost of one read of `client::Client` against the test server of open62541, each
//! on its own connection manager, over the sim net. Run with `cargo bench -p
//! connector-opcua --bench client`.
//!
//! - `client`: the ns in the polls of the read of a node by number, the CPU of the
//!   client.
//! - `round`: the ns from the first poll of that read to its value, with the server,
//!   the sim net, and the sim scheduler.
//! - `name client` and `name round`: the same for a read of a node by name.
//! - `bytes client` and `bytes round`: the same for a read of a `ByteString` whose
//!   answer takes 8 chunks.
//! - `client N` and `round N`: the same for one read of N nodes by number. A read of
//!   512 nodes sends 2 requests of [`PER_READ`] nodes, and a read of 4352 sends 17,
//!   more than the requests in flight.
//! - `sign client` and `sign round`, `encrypt client` and `encrypt round`: the same for
//!   a read of a node by number over a channel of `Basic256Sha256` in mode `Sign` and
//!   in mode `SignAndEncrypt`, each on its own server.
//!
//! [`PER_READ`]: connector_opcua::bench::client::PER_READ

use std::future::{Future as _, poll_fn};
use std::pin::pin;
use std::time::{Duration, Instant};

use connector_opcua::bench::client::{Client, Mode, Server, Variable};
use sim::Sim;

const WARMUP: usize = 10;
const ROUNDS: usize = 1000;

/// The node counts of the reads of a node by number.
const COUNTS: [usize; 5] = [1, 16, 256, 512, 4352];

fn main() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let mut lines = sim.run_on(&node, bench).expect("the run ends");
    print(&mut lines);
}

/// The name and the ns of the polls and of the whole of each timed read: of a node by
/// number, of a node by name, then of each other count of [`COUNTS`] by number, then of
/// a node by number over each secure mode.
async fn bench(node: sim::node::Node, _: env::tasks::Tasks) -> Vec<(String, Vec<u64>)> {
    let address = node.addresses()[0];
    let body = async |server: &Server| {
        let mut lines = Vec::new();
        for count in COUNTS {
            let mut client = server.connect(count).await;
            let [polls, rounds] = reads(&mut client, Variable::Number).await;
            let suffix = if count == 1 {
                String::new()
            } else {
                format!(" {count}")
            };
            lines.push((format!("client{suffix}"), polls));
            lines.push((format!("round{suffix}"), rounds));
            if count == 1 {
                let [polls, rounds] = reads(&mut client, Variable::Name).await;
                lines.push(("name client".to_owned(), polls));
                lines.push(("name round".to_owned(), rounds));
                let [polls, rounds] = reads(&mut client, Variable::Bytes).await;
                lines.push(("bytes client".to_owned(), polls));
                lines.push(("bytes round".to_owned(), rounds));
            }
            client.close().await;
        }
        lines
    };
    let rng = node.entropy().rng();
    let mut lines = Server::scope(node.clock(), node.net(), address, rng, body).await;
    for (mode, name) in [(Mode::Sign, "sign"), (Mode::SignAndEncrypt, "encrypt")] {
        let body = async |server: &Server| {
            let mut client = server.connect(1).await;
            let read = reads(&mut client, Variable::Number).await;
            client.close().await;
            read
        };
        let rng = node.entropy().rng();
        let (clock, net) = (node.clock(), node.net());
        let [polls, rounds] =
            Server::secure(clock, net, address, rng, mode, body).await;
        lines.push((format!("{name} client"), polls));
        lines.push((format!("{name} round"), rounds));
    }
    lines
}

/// The ns of the polls and of the whole of each timed read of `variable`.
async fn reads(client: &mut Client, variable: Variable) -> [Vec<u64>; 2] {
    let (mut polls, mut rounds) = (Vec::new(), Vec::new());
    for read in 0..WARMUP + ROUNDS {
        let mut future = pin!(client.read(variable));
        let mut spent = Duration::ZERO;
        let start = now();
        poll_fn(|cx| {
            let poll_start = now();
            let poll = future.as_mut().poll(cx);
            spent += now().duration_since(poll_start);
            poll
        })
        .await;
        let round = now().duration_since(start);
        if read >= WARMUP {
            polls.push(nanos(spent));
            rounds.push(nanos(round));
        }
    }
    [polls, rounds]
}

#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn now() -> Instant {
    Instant::now()
}

fn nanos(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).expect("a read takes under 2^64 ns")
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &mut [(String, Vec<u64>)]) {
    println!("ns per read over {ROUNDS} reads");
    println!("pN: the read at percentile N");
    println!("{:<14} {:>9} {:>9} {:>9}", "line", "p10", "p50", "p90");
    for (name, nanos) in lines {
        nanos.sort_unstable();
        let at = |percent: usize| nanos[ROUNDS * percent / 100];
        println!("{name:<14} {:>9} {:>9} {:>9}", at(10), at(50), at(90));
    }
}
