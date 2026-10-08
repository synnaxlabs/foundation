//! The cost of a `stream::Receiver::recv` on a session between two sim nodes. Run
//! with `cargo bench -p transport --bench recv`. A figure is the time of the polls of
//! `recv` per message over a round, and some messages in a round cost more than
//! others.
//!
//! Each round, the client sends 16 messages of one size on one `Complete` stream, and
//! the server times each poll of `recv` until it has them. A sim sleep before each
//! round lets the peer read and acknowledge. The window holds a round, so a message
//! waits for no credit. `polls/msg` near 1 means most messages were whole at their
//! first poll; a message that spans polls costs more. A poll also drives the session,
//! so it holds the cost of the packets it reads.
//!
//! To compare two builds, run each several times in turn on one pinned core and
//! compare p10 and p50: on a busy machine, p90 holds the preemptions.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Code, Config, Error, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
const MESSAGES: usize = 16;
const WARMUP: usize = 20;
const ROUNDS: usize = 200;
const PAUSE: Span = Span::from_nanos(100_000_000);
const CLOSED: Error = Error::PeerClosed { code: Code(0) };
/// A small message, the largest that fits in 64 chunks of a datagram, and one of
/// many more chunks.
const SIZES: [usize; 3] = [1 << 10, 56 << 10, 1 << 20];
const MESSAGE_BYTES_MAX: usize = 1 << 20;

/// Per size: the ns of `recv` polls per message of each round, the polls, and the
/// allocations.
type Results = Arc<Mutex<Vec<(Vec<f64>, u64, u64)>>>;

fn main() {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let results = serve(&server);
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(at)])
            .await
            .expect("a session");
        let clock = node.clock();
        for bytes in SIZES {
            let mut sender = session
                .open_sender(Class::Complete)
                .await
                .expect("a stream");
            for _ in 0..WARMUP + ROUNDS {
                clock.sleep(PAUSE).await;
                for _ in 0..MESSAGES {
                    sender.send(filled(&pool, bytes)).await.expect("sent");
                }
            }
            sender.finish().expect("finished");
        }
        clock.sleep(PAUSE).await;
        session.close(Code(0));
        clock.sleep(Span::MILLISECOND).await;
    })
    .expect("the bench runs");
    print(&results.lock().expect("not poisoned"));
}

fn serve(node: &Node) -> Results {
    let results = Results::default();
    let out = Arc::clone(&results);
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks.clone(), SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        for _ in SIZES {
            let mut receiver = match session.accept().await {
                Ok(incoming) => incoming.receiver,
                Err(error) => panic!("accept: {error}"),
            };
            let (mut rounds, mut polls, mut allocations) = (Vec::new(), 0, 0);
            'stream: loop {
                let mut spent = 0;
                for _ in 0..MESSAGES {
                    let mut recv = pin!(receiver.recv());
                    let read = future::poll_fn(|cx| {
                        let (poll, took, counted) = timed(|| recv.as_mut().poll(cx));
                        spent += took;
                        polls += 1;
                        allocations += counted;
                        poll
                    })
                    .await;
                    match read {
                        Ok(Some(message)) => drop::<Block>(message),
                        Ok(None) => break 'stream,
                        Err(error) => panic!("read: {error}"),
                    }
                }
                rounds.push(per(spent) / per(MESSAGES));
            }
            out.lock()
                .expect("not poisoned")
                .push((rounds, polls, allocations));
        }
        assert_eq!(session.closed().await, CLOSED, "the client closes");
    });
    drop(started.expect("a shard"));
    results
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(results: &[(Vec<f64>, u64, u64)]) {
    println!("ns of recv polls per message, over {ROUNDS} rounds of {MESSAGES}");
    println!(
        "{:<10} {:>9} {:>9} {:>9} {:>10} {:>11}",
        "bytes", "p10", "p50", "p90", "polls/msg", "allocs/msg"
    );
    let messages = per((WARMUP + ROUNDS) * MESSAGES);
    for (bytes, (nanos, polls, allocations)) in SIZES.iter().zip(results) {
        let mut nanos = nanos[WARMUP..].to_vec();
        nanos.sort_unstable_by(f64::total_cmp);
        let at = |p: usize| nanos[ROUNDS * p / 100];
        println!(
            "{bytes:<10} {:>9.1} {:>9.1} {:>9.1} {:>10.2} {:>11.2}",
            at(10),
            at(50),
            at(90),
            per(*polls) / messages,
            per(*allocations) / messages,
        );
    }
}

/// Runs `f`, and returns its result, its ns, and its allocations.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn timed<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    let start = Instant::now();
    let (value, counted) = ALLOCATOR.count(f);
    let nanos = u64::try_from(Instant::now().duration_since(start).as_nanos())
        .expect("a poll ends within 584 years");
    (value, nanos, counted)
}

#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "a printed figure loses no digit it shows"
)]
fn per(value: impl TryInto<u64, Error: std::fmt::Debug>) -> f64 {
    value.try_into().expect("fits 64 bits") as f64
}

fn filled(pool: &Pool, bytes: usize) -> Block {
    let mut block = pool.alloc(bytes).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let pool = block::Config { budget: 1 << 26 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(MESSAGE_BYTES_MAX).expect("not zero"),
        window_bytes: MESSAGES * MESSAGE_BYTES_MAX * 2,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(10 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool: Rc::new(Pool::new(pool, memory)),
    }
}

fn part(node: &Node, port: u16) -> transport::port::Part {
    let at = SocketAddr::new(node.addresses()[0], port);
    let port = Port::bind(&node.net(), at).expect("a port");
    port.split(NonZeroUsize::MIN).pop().expect("one part")
}

fn public(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("32 bytes");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("aws-lc makes no key of small order")
}
