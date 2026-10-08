//! After a read that gives a whole long message, in one poll or over many, the
//! receiver keeps a list of at most 64 chunks, not one sized by the message, both
//! before and after it reads the end of the stream. The count covers each thread, so
//! this binary has no test harness. The sim runs on one thread, so the count is exact.

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
use env::clock::Clock;
use sim::Sim;
use sim::node::Node;
use transport::stream::Receiver;
use transport::{Address, Class, Config, Error, Port, Transport};
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
/// When the server reads after it accepts the stream, once the message is in.
const READ: Span = Span::from_nanos(1_000_000_000);
/// When the client resets the stream after its send: past the server's read.
const RESET: Span = Span::from_nanos(1_500_000_000);
/// How long the server waits after its read of a stream that the client resets,
/// before it drops the receiver: past the reset.
const DROP: Span = Span::from_nanos(2_000_000_000);
/// How long the client lives after it ends the stream: past the server's drop.
const LIVE: Span = Span::from_nanos(3_000_000_000);

/// How the server reads the message.
#[derive(Clone, Copy, Debug)]
enum Reading {
    /// In one poll, once the message is in.
    Whole,
    /// Each millisecond from when the stream comes, until the message is in.
    Parts,
}

/// How the client ends the stream after its message.
#[derive(Clone, Copy, Debug)]
enum End {
    /// It finishes the stream, and the server reads the end before the drop.
    Finish,
    /// It resets the stream after the server's read, and the server drops the
    /// receiver with no more reads. The reset stream needs no stop, so the drop
    /// allocates nothing.
    Reset,
}

/// What the server's reads give.
#[derive(Clone, Copy, Debug, Default)]
struct Out {
    /// The length of the message that the read gives.
    len: Option<usize>,
    /// The polls of the read that give `Pending`.
    pending: usize,
    /// Whether the server read the end of the stream after the message.
    ended: bool,
    /// The net heap bytes that the drop of the receiver then gives back.
    kept: usize,
}

fn main() {
    for reading in [Reading::Whole, Reading::Parts] {
        for (len, end) in [100_000, 240_000, 1 << 18]
            .into_iter()
            .flat_map(|len| [(len, End::Finish), (len, End::Reset)])
        {
            let out = run(reading, len, end);
            assert_eq!(out.len, Some(len), "{reading:?}, {len} bytes: the read");
            assert_eq!(
                out.pending > 0,
                matches!(reading, Reading::Parts),
                "{reading:?}, {len} bytes: the read gives `Pending` {} times",
                out.pending
            );
            assert_eq!(
                out.ended,
                matches!(end, End::Finish),
                "{reading:?}, {len} bytes, {end:?}: the read of the end of the stream"
            );
            assert!(
                out.kept <= KEPT_MAX,
                "{reading:?}, {len} bytes, {end:?}: the receiver keeps {} bytes \
                 after a whole message",
                out.kept
            );
        }
    }
}

/// The [`Out`] of the server's read of a message of `len` bytes in the way of
/// `reading`, on a stream that the client ends in the way of `end`.
fn run(reading: Reading, len: usize, end: End) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new(Out::default()));
    serve(&server, reading, end, Arc::clone(&out));
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(address)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        sender.send(filled(&pool, len)).await.expect("sent");
        match end {
            End::Finish => sender.finish().expect("finished"),
            End::Reset => {
                node.clock().sleep(RESET).await;
                drop(sender);
            }
        }
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    *out.lock().expect("not poisoned")
}

/// Starts the server on `node`. It reads the message in the way of `reading`, then,
/// as `end` says, the end once it comes or nothing until the reset. It then drops the
/// receiver and puts the [`Out`] in `out`.
fn serve(node: &Node, reading: Reading, end: End, out: Arc<Mutex<Out>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        if let Reading::Whole = reading {
            clock.sleep(READ).await;
        }
        let (read, pending) = next(&mut receiver, &clock).await;
        let len = read.ok().flatten().map(|block| block.len());
        let ended = match end {
            End::Finish => matches!(next(&mut receiver, &clock).await.0, Ok(None)),
            End::Reset => {
                clock.sleep(DROP).await;
                false
            }
        };
        let before = ALLOCATOR.held();
        drop(receiver);
        let kept = before.saturating_sub(ALLOCATOR.held());
        *out.lock().expect("not poisoned") = Out {
            len,
            pending,
            ended,
            kept,
        };
    });
    drop(started.expect("a shard"));
}

/// Polls one `receiver.recv()` each millisecond until it is ready. Gives the read and
/// the polls that gave `Pending`.
async fn next(
    receiver: &mut Receiver,
    clock: &Clock,
) -> (Result<Option<Block>, Error>, usize) {
    let mut recv = pin!(receiver.recv());
    let mut pending = 0;
    loop {
        if let Poll::Ready(read) =
            poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx))).await
        {
            return (read, pending);
        }
        pending += 1;
        clock.sleep(Span::MILLISECOND).await;
    }
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
