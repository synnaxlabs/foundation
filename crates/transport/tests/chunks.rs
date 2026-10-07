//! A read holds at most 64 of the chunks that a message comes in. Each time the list
//! is full, the read copies it into one heap buffer, so a message in many tiny
//! frames cannot grow the list. A packet holds at most 1472 bytes, and noq-proto
//! keeps each packet's bytes in place until their spare bytes pass the larger of
//! 32 KiB and 1.5 times the bytes it holds, which these messages do not reach. So
//! the only heap block that holds all of a longer pattern is that buffer. A read of
//! 65 chunks also makes one allocation more than a read of 64: the buffer, and no
//! larger list. The counts cover each thread, so this binary has no test harness.
//! The sim runs on one thread, so the counts are exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Config, Error, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
/// The bytes of the pattern, longer than a packet.
const PATTERN: usize = 4096;
/// A message that comes in 64 chunks in this sim, a full list that the read never
/// copies.
const FULL: usize = 83_600;
/// A message that comes in 65 chunks in this sim, one past a full list.
const PAST: usize = 84_900;
/// Where the pattern starts in a long message. Each packet but the last carries
/// between 1000 and 1472 bytes of a message, so the pattern lies past its first 64
/// packets and inside its first 128: only a second copy of a full list holds it.
const SECOND: usize = 110_000;
/// Where the pattern starts in a message of 1 << 18 bytes: past its first 128
/// packets, and in fewer than 64 after them, so no copy of a full list holds it.
const LAST: usize = 190_000;
/// When the server reads after it accepts the stream, once the whole message is in.
const READ: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its send: past the server's read.
const LIVE: Span = Span::from_nanos(2_000_000_000);

/// The server's one poll of its read, the heap blocks that hold the pattern that the
/// poll frees, and the allocations the poll makes.
type Out = (Poll<Result<Option<Vec<u8>>, Error>>, u64, u64);

fn main() {
    let pattern: Vec<u8> = (0..=250).cycle().take(PATTERN).collect();
    let cases = [
        (FULL, 0, 0),
        (PAST, 0, 1),
        (240_000, SECOND, 1),
        (1 << 18, LAST, 0),
    ];
    let mut allocations = Vec::new();
    for (len, at, copies) in cases {
        let (read, freed, allocated) = run(&pattern, len, at);
        allocations.push(allocated);
        let Poll::Ready(Ok(Some(read))) = read else {
            panic!("{len} bytes: the read gave {read:?}");
        };
        assert_eq!(read.len(), len, "{len} bytes: the message's length");
        let end = at.saturating_add(PATTERN);
        assert_eq!(read[at..end], pattern, "{len} bytes: the pattern");
        assert_eq!(
            freed, copies,
            "{len} bytes: the heap buffers that the read frees with the whole pattern"
        );
    }
    let [full, past, ..] = allocations[..] else {
        unreachable!("four cases")
    };
    assert_eq!(
        past.checked_sub(full),
        Some(1),
        "the copy of a full list allocates only its buffer"
    );
}

/// The [`Out`] of the server's read of a message of `len` bytes with `pattern` at
/// `at`.
fn run(pattern: &[u8], len: usize, at: usize) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((Poll::Pending, 0, 0)));
    serve(&server, pattern.to_vec(), Arc::clone(&out));
    let message = pattern.to_vec();
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
        sender
            .send(filled(&pool, &message, len, at))
            .await
            .expect("sent");
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    let out = out.lock().expect("not poisoned");
    (out.0.clone(), out.1, out.2)
}

/// Starts the server on `node`. Once the whole message is in, it reads it in one
/// poll and puts the [`Out`] of that poll for `pattern` in `out`.
fn serve(node: &Node, pattern: Vec<u8>, out: Arc<Mutex<Out>>) {
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
        own.clock().sleep(READ).await;
        let ((read, freed), allocated) = {
            let mut recv = pin!(receiver.recv());
            let mut cx = Context::from_waker(Waker::noop());
            ALLOCATOR.count(|| {
                ALLOCATOR.freed_holding(&pattern, || recv.as_mut().poll(&mut cx))
            })
        };
        let message =
            read.map(|read| read.map(|block| block.map(|block| block.to_vec())));
        *out.lock().expect("not poisoned") = (message, freed, allocated);
        drop(receiver);
    });
    drop(started.expect("a shard"));
}

/// A block of `len` bytes from `pool` with `pattern` at `at`.
fn filled(pool: &Pool, pattern: &[u8], len: usize, at: usize) -> Block {
    let mut block = pool.alloc(len).expect("the pool has room");
    block.fill(0x5a);
    block[at..at.saturating_add(pattern.len())].copy_from_slice(pattern);
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
