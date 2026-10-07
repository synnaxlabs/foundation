//! A `send_parts` allocates as a `send` of one block of the same bytes, plus one
//! allocation for each copied stretch over 1452 bytes and for each growth of the
//! segment queue of noq-proto. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::stream::{Part, Sender};
use transport::{Address, Class, Code, Config, Error, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
/// Long enough for the peer to read and acknowledge a message.
const PAUSE: Span = Span::from_nanos(100_000_000);
const CLOSED: Error = Error::PeerClosed { code: Code(0) };
/// The sends of each kind before the counted ones.
const WARMUP: usize = 8;
const ROUNDS: usize = 8;

/// The parts of a message: `ranges` ranges of `len` bytes, `stride` bytes apart, and
/// the allocations of its `send_parts` over those of a `send`, with their cause.
struct Shape {
    ranges: usize,
    len: usize,
    stride: usize,
    over: u64,
    cause: &'static str,
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    serve(&server);
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(at)])
            .await
            .expect("a session");
        let clock = node.clock();
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        let shapes = [
            Shape {
                ranges: 3,
                len: 8 << 10,
                stride: (8 << 10) + 8,
                over: 0,
                cause: "none: the segment queue holds the header and 3 slices",
            },
            // noq-proto drops its segment queue once the peer acknowledges it all, so
            // each message grows it again.
            Shape {
                ranges: 8,
                len: 8 << 10,
                stride: (8 << 10) + 8,
                over: 2,
                cause: "the segment queue grows to 8, then 16, for the header and slices",
            },
            Shape {
                ranges: 1000,
                len: 8,
                stride: 16,
                over: 1,
                cause: "the copy of the stretch",
            },
        ];
        for shape in &shapes {
            let parts: Vec<Part> = (0..shape.ranges)
                .map(|at| Part {
                    range: at * shape.stride..at * shape.stride + shape.len,
                    zeros: 0,
                })
                .collect();
            let bytes = shape.ranges * shape.len;
            let large = shape.ranges * shape.stride;
            for round in 0..WARMUP + ROUNDS {
                clock.sleep(PAUSE).await;
                let block = filled(&pool, bytes);
                let sent = ALLOCATOR.count(|| poll(&mut sender, block, &[])).1;
                clock.sleep(PAUSE).await;
                let block = filled(&pool, large);
                let parted = ALLOCATOR.count(|| poll(&mut sender, block, &parts)).1;
                if round >= WARMUP {
                    assert_eq!(
                        parted,
                        sent + shape.over,
                        "{} ranges of {} bytes, over send: {}",
                        shape.ranges,
                        shape.len,
                        shape.cause
                    );
                }
            }
        }
        sender.finish().expect("finished");
        clock.sleep(PAUSE).await;
        session.close(Code(0));
        clock.sleep(Span::MILLISECOND).await;
    })
    .expect("the test runs");
}

/// Polls a send of `parts` of `block` once, or of all of it when `parts` is empty.
///
/// # Panics
///
/// When the send waits or fails.
fn poll(sender: &mut Sender, block: Block, parts: &[Part]) {
    let mut cx = Context::from_waker(Waker::noop());
    let polled = if parts.is_empty() {
        pin!(sender.send(block)).poll(&mut cx)
    } else {
        pin!(sender.send_parts(block, parts)).poll(&mut cx)
    };
    match polled {
        Poll::Ready(sent) => sent.expect("the send goes"),
        Poll::Pending => panic!("a send waited on its first poll"),
    }
}

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
        let mut receiver = session.accept().await.expect("a stream").receiver;
        loop {
            match receiver.recv().await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => panic!("the server failed to read: {error}"),
            }
        }
        assert_eq!(session.closed().await, CLOSED);
    });
    drop(started.expect("a shard"));
}

fn filled(pool: &Pool, bytes: usize) -> Block {
    let mut block = pool.alloc(bytes).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let pool = block::Config { budget: 1 << 22 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(1 << 17).expect("not zero"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(4).expect("not zero"),
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
