//! The cost of a `stream::Sender::send` that is ready on its first poll, on a session
//! between two sim nodes. Run with `cargo bench -p transport --bench send`.
//!
//! Each round fills its blocks, then times one poll of each send. A sim sleep between
//! rounds lets the peer read and acknowledge. A send that waits on its first poll
//! panics, so no number holds a wait.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::stream::Sender;
use transport::{Address, Class, Code, Config, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;

/// Sends per round. Each round fits the window, so no send waits.
const SENDS: usize = 64;
/// Rounds per scenario before the timed rounds.
const WARMUP: usize = 50;
/// Timed rounds per scenario.
const ROUNDS: usize = 500;
/// The sim time between rounds, in which the peer reads and acknowledges a round.
const PAUSE: Span = Span::from_nanos(100_000_000);

/// What one scenario sends: one stream per class, in turn, or no stream for the
/// control, which moves each block into a list in place of a send.
struct Scenario {
    name: &'static str,
    classes: &'static [Class],
    bytes: usize,
}

const SCENARIOS: [Scenario; 6] = [
    Scenario {
        name: "control 1 KiB",
        classes: &[],
        bytes: 1024,
    },
    Scenario {
        name: "complete 64 B",
        classes: &[Class::Complete],
        bytes: 64,
    },
    Scenario {
        name: "complete 1 KiB",
        classes: &[Class::Complete],
        bytes: 1024,
    },
    Scenario {
        name: "complete 16 KiB",
        classes: &[Class::Complete],
        bytes: 16 * 1024,
    },
    Scenario {
        name: "latest and complete 1 KiB",
        classes: &[Class::Latest, Class::Complete],
        bytes: 1024,
    },
    Scenario {
        name: "8 complete 1 KiB",
        classes: &[Class::Complete; 8],
        bytes: 1024,
    },
];

/// The result of one scenario.
struct Line {
    name: &'static str,
    /// The nanoseconds of each timed round, sorted.
    nanos: Vec<usize>,
    allocations: u64,
}

fn main() {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    serve(&server);
    let lines = sim
        .run_on(&client, move |node, tasks| async move {
            let config = config(&node, tasks, CLIENT);
            let pool = Rc::clone(&config.pool);
            let transport =
                Transport::new(config, part(&node, 0)).expect("a transport");
            let session = transport
                .dial(public(&SERVER), &[Address::Udp(at)])
                .await
                .expect("a session");
            let clock = node.clock();
            let mut lines = Vec::with_capacity(SCENARIOS.len());
            for scenario in &SCENARIOS {
                let mut senders = Vec::with_capacity(scenario.classes.len());
                for &class in scenario.classes {
                    senders.push(session.open_sender(class).await.expect("a stream"));
                }
                lines.push(measure(&clock, &pool, &mut senders, scenario).await);
                for sender in &mut senders {
                    sender.finish().expect("the stream finishes");
                }
            }
            session.close(Code(0));
            // A shard that ends drops its tasks, so give the close time to go out.
            clock.sleep(Span::MILLISECOND).await;
            lines
        })
        .expect("the bench runs");
    print(&lines);
}

/// Starts a shard on `node` that accepts one session and reads each stream until the
/// session closes.
fn serve(node: &Node) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks.clone(), SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        while let Ok(incoming) = session.accept().await {
            let mut receiver = incoming.receiver;
            tasks
                .spawn(async move { while let Ok(Some(_)) = receiver.recv().await {} });
        }
    });
    drop(started.expect("a shard"));
}

/// Runs the rounds of `scenario` on `senders` and gives its line.
async fn measure(
    clock: &env::clock::Clock,
    pool: &Pool,
    senders: &mut [Sender],
    scenario: &Scenario,
) -> Line {
    let mut nanos = Vec::with_capacity(ROUNDS);
    let mut allocations = 0;
    let mut held = Vec::with_capacity(SENDS);
    for round in 0..WARMUP + ROUNDS {
        clock.sleep(PAUSE).await;
        let blocks: Vec<Block> =
            (0..SENDS).map(|_| filled(pool, scenario.bytes)).collect();
        let (span, counted) = ALLOCATOR.count(|| time(senders, blocks, &mut held));
        held.clear();
        if round >= WARMUP {
            nanos.push(span);
            allocations += counted;
        }
    }
    nanos.sort_unstable();
    Line {
        name: scenario.name,
        nanos,
        allocations,
    }
}

/// Polls one send of each block once, on `senders` in turn, and gives the nanoseconds
/// it took. With no sender, it moves each block into `held` in place of a send.
///
/// # Panics
///
/// When a send waits or fails.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn time(senders: &mut [Sender], blocks: Vec<Block>, held: &mut Vec<Block>) -> usize {
    let mut cx = Context::from_waker(Waker::noop());
    let mut blocks = blocks.into_iter();
    let start = Instant::now();
    if senders.is_empty() {
        held.extend(std::hint::black_box(&mut blocks));
    }
    while blocks.len() > 0 {
        for (sender, block) in senders.iter_mut().zip(&mut blocks) {
            match pin!(sender.send(block)).poll(&mut cx) {
                Poll::Ready(sent) => sent.expect("the send goes"),
                Poll::Pending => panic!("a send waited on its first poll: raise PAUSE"),
            }
        }
    }
    let nanos = Instant::now().duration_since(start).as_nanos();
    usize::try_from(nanos).expect("a round takes under 2^64 ns")
}

/// A block from `pool` of `bytes` bytes.
fn filled(pool: &Pool, bytes: usize) -> Block {
    let mut block = pool.alloc(bytes).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

/// A config for `key` on `node`, with room for a round of the largest message in
/// flight.
fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let memory = Heap::new(block::Config { budget: 1 << 24 }.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
        window_bytes: 1 << 22,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(10 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool: Rc::new(Pool::new(block::Config { budget: 1 << 24 }, memory)),
    }
}

/// The one part of a port bound at `port` on the first IP of `node`.
fn part(node: &Node, port: u16) -> transport::port::Part {
    let at = SocketAddr::new(node.addresses()[0], port);
    let port = Port::bind(&node.net(), at).expect("a port");
    port.split(NonZeroUsize::MIN).pop().expect("one part")
}

/// The public key of `key`.
fn public(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("32 bytes");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("aws-lc makes no key of small order")
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ns per send over {ROUNDS} rounds of {SENDS} sends");
    println!(
        "{:<28} {:>9} {:>9} {:>12}",
        "scenario", "p50", "mean", "allocs/send"
    );
    let sends = float(ROUNDS * SENDS);
    for line in lines {
        let p50 = float(line.nanos[ROUNDS / 2]) / float(SENDS);
        let mean = float(line.nanos.iter().sum()) / sends;
        let allocations = line.allocations;
        let allocations = float(usize::try_from(allocations).expect("fits")) / sends;
        println!(
            "{:<28} {p50:>9.1} {mean:>9.1} {allocations:>12.2}",
            line.name
        );
    }
}

fn float(value: usize) -> f64 {
    f64::from(u32::try_from(value).expect("a value under 2^32"))
}
