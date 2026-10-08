//! The receiver of a stream that this node opens keeps a list of at most 64 chunks
//! after a whole message of many packets, as an accepted one does. It reads with a
//! reader that starts with no byte of the stream and an empty list, in one poll once
//! the reply is in, and also with one poll each millisecond from the open, which
//! waits for the prefix of the reply. The count covers each thread, so this binary
//! has no test harness. The sim runs on one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::poll_fn;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Config, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
/// The most heap that the drop of the receiver gives back: a list of 64 chunks, since
/// each slot is 32 bytes.
const KEPT_MAX: usize = 2 << 10;
/// The bytes of the client's message, which opens the stream at the server.
const SHORT: usize = 1000;
/// When the server replies after it reads the client's message.
const REPLY: Span = Span::from_nanos(1_000_000_000);
/// When the client reads in one poll after its send: past the reply.
const READ: Span = Span::from_nanos(1_300_000_000);
/// When the server resets the stream after its reply: past the client's read.
const END: Span = Span::from_nanos(1_500_000_000);
/// How long the client waits after its read before it drops the receiver: past the
/// reset, so the drop needs no stop and allocates nothing.
const DROP: Span = Span::from_nanos(2_000_000_000);
/// How long each side lives after its last step: past the other side's.
const LIVE: Span = Span::from_nanos(4_000_000_000);

/// How the client reads the reply.
#[derive(Clone, Copy, Debug)]
enum Reading {
    /// In one poll, once the reply is in.
    Whole,
    /// Each millisecond from the open, until the reply is in.
    Parts,
}

/// What the client's read gives.
#[derive(Clone, Copy, Debug, Default)]
struct Out {
    /// The length of the reply.
    len: Option<usize>,
    /// The polls of the read that give `Pending`.
    pending: usize,
    /// The net heap bytes that the drop of the receiver then gives back.
    kept: usize,
}

fn main() {
    for reading in [Reading::Whole, Reading::Parts] {
        for len in [60_000, 100_000, 240_000, 1 << 18] {
            let out = run(reading, len);
            assert_eq!(out.len, Some(len), "{reading:?}, {len} bytes: the read");
            assert_eq!(
                out.pending > 0,
                matches!(reading, Reading::Parts),
                "{reading:?}, {len} bytes: the read gives `Pending` {} times",
                out.pending
            );
            assert!(
                out.kept <= KEPT_MAX,
                "{reading:?}, {len} bytes: the receiver keeps {} bytes after a whole \
                 message",
                out.kept
            );
        }
    }
}

/// The [`Out`] of the client's read of a reply of `len` bytes, in the way of
/// `reading`, on a stream that it opens.
fn run(reading: Reading, len: usize) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    serve(&server, len);
    let out = Arc::new(Mutex::new(Out::default()));
    let result = Arc::clone(&out);
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(address)])
            .await
            .expect("a session");
        let (mut sender, mut receiver) =
            session.open(Class::Complete).await.expect("a stream");
        sender.send(filled(&pool, SHORT)).await.expect("sent");
        let clock = node.clock();
        if matches!(reading, Reading::Whole) {
            clock.sleep(READ).await;
        }
        let mut pending = 0;
        let read = {
            let mut recv = pin!(receiver.recv());
            loop {
                if let Poll::Ready(read) =
                    poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx))).await
                {
                    break read;
                }
                pending += 1;
                clock.sleep(Span::MILLISECOND).await;
            }
        };
        let len = read.ok().flatten().map(|block| block.len());
        clock.sleep(DROP).await;
        let before = ALLOCATOR.held();
        drop(receiver);
        let kept = before.saturating_sub(ALLOCATOR.held());
        *result.lock().expect("not poisoned") = Out { len, pending, kept };
        clock.sleep(LIVE).await;
        drop(sender);
    })
    .expect("the run ends");
    *out.lock().expect("not poisoned")
}

/// Starts the server on `node`. It reads the client's message, replies with one of
/// `len` bytes, and then resets the stream.
fn serve(node: &Node, len: usize) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let incoming = session.accept().await.expect("a stream");
        let mut receiver = incoming.receiver;
        let mut reply = incoming.sender.expect("a two-way stream");
        let short = receiver.recv().await.expect("a read").expect("a message");
        assert_eq!(short.len(), SHORT, "the client's message");
        own.clock().sleep(REPLY).await;
        reply.send(filled(&pool, len)).await.expect("sent");
        own.clock().sleep(END).await;
        drop(reply);
        own.clock().sleep(LIVE).await;
        drop(receiver);
    });
    drop(started.expect("a shard"));
}

/// A block of `len` bytes from `pool`.
fn filled(pool: &Pool, len: usize) -> Block {
    let mut block = pool.alloc(len).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let pool = block::Config { budget: 1 << 20 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(1 << 18).expect("not zero"),
        window_bytes: 1 << 20,
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
