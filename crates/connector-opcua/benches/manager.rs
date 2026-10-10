//! The cost of the drives of `connection::Manager` with the test server of open62541
//! and 1, 16, or 64 clients on its loop, over the sim net. Run with `cargo bench -p
//! connector-opcua --bench manager`.
//!
//! Each round times one read of the first client, in three drives with a sim sleep of
//! one link delay before the second and the third, then a fourth drive:
//!
//! - `ask`: the drive whose run asks for the read, so it sends the request.
//! - `answer`: the drive in which the server reads the request and sends the answer.
//! - `receive`: the drive in which the client reads the answer.
//! - `pass`: a drive with nothing ready. It is the cost of a pass over each
//!   connection, which each other drive also pays.
//!
//! The other clients hold a session and send nothing. With the listen connection, the
//! manager holds 3, 33, or 129 connections. A figure is the ns of one drive.

use std::time::{Duration, Instant};

use connector_opcua::bench::Manager;
use sim::Sim;
use types::time::Span;

/// The idle clients of each run.
const IDLE: [usize; 3] = [0, 15, 63];
/// The delay of the default link.
fn delay() -> Span {
    sim::link::Config::default().delay
}
const WARMUP: usize = 10;
const ROUNDS: usize = 1000;
/// The lines of each run, in the order of a round.
const LINES: [&str; 4] = ["ask", "answer", "receive", "pass"];

fn main() {
    let mut lines = Vec::new();
    for idle in IDLE {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let run = move |node: sim::node::Node, _| bench(node, idle);
        let nanos = sim.run_on(&node, run).expect("the run ends");
        lines.extend(
            LINES
                .into_iter()
                .zip(nanos)
                .map(|(name, nanos)| (format!("{name} {}", idle + 1), nanos)),
        );
    }
    print(&mut lines);
}

/// The ns of each drive of the timed rounds, one list for each of [`LINES`].
async fn bench(node: sim::node::Node, idle: usize) -> [Vec<u64>; 4] {
    let (clock, address) = (node.clock(), node.addresses()[0]);
    let body = async |manager: &Manager| {
        let mut nanos: [Vec<u64>; 4] = Default::default();
        for round in 0..WARMUP + ROUNDS {
            let ask = timed(|| manager.ask());
            clock.sleep(delay()).await;
            let answer = timed(|| manager.drive());
            clock.sleep(delay()).await;
            let receive = timed(|| manager.drive());
            let pass = timed(|| manager.drive());
            assert_eq!(manager.answers(), round + 1, "each round reads once");
            if round >= WARMUP {
                let values = [ask, answer, receive, pass];
                for (line, value) in nanos.iter_mut().zip(values) {
                    line.push(value);
                }
            }
        }
        nanos
    };
    Manager::scope(node.clock(), node.net(), address, idle, body).await
}

/// The ns that `f` takes.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn timed(f: impl FnOnce()) -> u64 {
    let start = Instant::now();
    f();
    nanos(Instant::now().duration_since(start))
}

fn nanos(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).expect("a drive takes under 2^64 ns")
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &mut [(String, Vec<u64>)]) {
    println!("ns per drive over {ROUNDS} rounds; N: the clients");
    println!("pN: the drive at percentile N");
    println!("{:<12} {:>9} {:>9} {:>9}", "line", "p10", "p50", "p90");
    for (name, nanos) in lines {
        nanos.sort_unstable();
        let at = |percent: usize| nanos[ROUNDS * percent / 100];
        println!("{name:<12} {:>9} {:>9} {:>9}", at(10), at(50), at(90));
    }
}
