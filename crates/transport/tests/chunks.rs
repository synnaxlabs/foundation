//! A read holds at most a fixed number of the chunks that a message comes in. Past
//! it, the read copies them into one heap buffer, so a message in many tiny frames
//! cannot grow its list of chunks. A packet holds at most 1472 bytes, so the only
//! heap block that holds all of a longer pattern is that buffer. The count covers
//! each thread, so this binary has no test harness. The sim runs on one thread, so
//! the count is exact.

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
use transport::{Address, Class, Config, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
/// The bytes at the start of each message, longer than a packet.
const PATTERN: usize = 4096;
/// When the server reads, after the whole message is in.
const READ: Span = Span::from_nanos(1_000_000_000);

fn main() {
    let pattern: Vec<u8> = (0..=250).cycle().take(PATTERN).collect();
    for (len, copies) in [(20_000, 0), (200_000, 1)] {
        let (read, freed) = run(&pattern, len);
        let read = read.expect("a message");
        assert_eq!(read.len(), len, "{len} bytes: the message's length");
        assert_eq!(read[..PATTERN], pattern, "{len} bytes: the message's start");
        assert_eq!(
            freed, copies,
            "{len} bytes: the heap buffers that the read frees with the whole pattern"
        );
    }
}

/// The message of `len` bytes that starts with `pattern` as the server reads it in
/// one poll, and the heap blocks that hold `pattern` that the poll frees.
fn run(pattern: &[u8], len: usize) -> (Option<Vec<u8>>, u64) {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0)));
    serve(&server, pattern.to_vec(), Arc::clone(&out));
    let message = pattern.to_vec();
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(at)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        sender
            .send(filled(&pool, &message, len))
            .await
            .expect("sent");
        let clock = node.clock();
        clock.sleep(READ).await;
        clock.sleep(READ).await;
    })
    .expect("the run ends");
    let out = out.lock().expect("not poisoned");
    (out.0.clone(), out.1)
}

/// Starts the server on `node`. Once the whole message is in, it reads it in one
/// poll and puts the message and the heap blocks that hold `pattern` that the poll
/// freed in `out`.
fn serve(node: &Node, pattern: Vec<u8>, out: Arc<Mutex<(Option<Vec<u8>>, u64)>>) {
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
        let (read, freed) = {
            let mut recv = pin!(receiver.recv());
            let mut cx = Context::from_waker(Waker::noop());
            ALLOCATOR.freed_holding(&pattern, || recv.as_mut().poll(&mut cx))
        };
        let message = match read {
            Poll::Ready(Ok(Some(block))) => Some(block.to_vec()),
            _ => None,
        };
        *out.lock().expect("not poisoned") = (message, freed);
        drop(receiver);
    });
    drop(started.expect("a shard"));
}

/// A block of `len` bytes from `pool` that starts with `start`.
fn filled(pool: &Pool, start: &[u8], len: usize) -> Block {
    let mut block = pool.alloc(len).expect("the pool has room");
    block.fill(0x5a);
    block[..start.len()].copy_from_slice(start);
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
