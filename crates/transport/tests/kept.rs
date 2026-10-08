//! After a read that gives a whole long message, the receiver keeps a list of at
//! most 64 chunks, not one sized by the message. The count covers each thread, so
//! this binary has no test harness. The sim runs on one thread, so the count is
//! exact.

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
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
/// The most net heap that the drop of the receiver gives back after the read: a list
/// of 64 chunks, since each slot is 32 bytes. The drop also allocates a few bytes.
const KEPT_MAX: usize = 2 << 10;
/// When the server reads after it accepts the stream, once the message is in.
const READ: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its send: past the server's read.
const LIVE: Span = Span::from_nanos(2_000_000_000);

/// The length of the message that the server's one poll gives, and the net heap
/// bytes that the drop of the receiver then gives back.
type Out = (Option<usize>, usize);

fn main() {
    for len in [100_000, 240_000, 1 << 18] {
        let (read, kept) = run(len);
        assert_eq!(read, Some(len), "{len} bytes: the read");
        assert!(
            kept <= KEPT_MAX,
            "{len} bytes: the receiver keeps {kept} bytes after a whole message"
        );
    }
}

/// The [`Out`] of the server's read of a message of `len` bytes.
fn run(len: usize) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0)));
    serve(&server, Arc::clone(&out));
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
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    *out.lock().expect("not poisoned")
}

/// Starts the server on `node`. Once the message is in, it reads it in one poll,
/// drops the receiver, and puts the [`Out`] in `out`.
fn serve(node: &Node, out: Arc<Mutex<Out>>) {
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
        let poll = {
            let mut recv = pin!(receiver.recv());
            let mut cx = Context::from_waker(Waker::noop());
            recv.as_mut().poll(&mut cx)
        };
        let len = match poll {
            Poll::Ready(Ok(Some(block))) => Some(block.len()),
            _ => None,
        };
        let before = ALLOCATOR.held();
        drop(receiver);
        let kept = before.saturating_sub(ALLOCATOR.held());
        *out.lock().expect("not poisoned") = (len, kept);
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
