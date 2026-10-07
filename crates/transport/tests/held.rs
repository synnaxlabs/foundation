//! A message that waits for a block from the pool is held on the heap, outside the
//! pool. When its stream resets or its session closes, the read that gives the error
//! frees it, though the caller keeps the receiver. The count covers each thread, so
//! this binary has no test harness. The sim runs on one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::poll_fn;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool, Unique};
use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Code, Config, Error, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const PORT: u16 = 4433;
const LEN: usize = 60_000;
const MESSAGE_BYTES_MAX: usize = 1 << 16;
/// When the client ends the stream or the session, after its send.
const END: Span = Span::from_nanos(500_000_000);
/// When the server reads the end, after the message waits for a block.
const READ: Span = Span::from_nanos(1_000_000_000);

/// How the client ends the message that waits.
#[derive(Clone, Copy, Debug)]
enum End {
    /// It drops its sender, which resets the stream with code 0.
    Reset,
    /// It closes the session with code 7.
    Close,
}

fn main() {
    for (end, error) in [
        (End::Reset, Error::Reset { code: Code(0) }),
        (End::Close, Error::PeerClosed { code: Code(7) }),
    ] {
        let (read, freed) = run(end);
        assert_eq!(read, Some(error), "{end:?}: the read after the end");
        assert_eq!(
            freed, LEN,
            "{end:?}: the read that gives the error frees the buffer of the message, \
             made at its length"
        );
    }
}

/// The error of the server's read after the client ends the stream in the way of
/// `end`, and the heap bytes that read freed.
fn run(end: End) -> (Option<Error>, usize) {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0)));
    serve(&server, Arc::clone(&out));
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
        sender.send(filled(&pool, LEN)).await.expect("sent");
        let clock = node.clock();
        clock.sleep(END).await;
        match end {
            End::Reset => drop(sender),
            End::Close => session.close(Code(7)),
        }
        clock.sleep(READ).await;
        clock.sleep(END).await;
    })
    .expect("the run ends");
    let out = out.lock().expect("not poisoned");
    (out.0.clone(), out.1)
}

/// Starts the server on `node`. It fills its pool, lets the client's message wait for
/// a block, and puts the error of its read after the end and the bytes that read
/// freed in `out`.
fn serve(node: &Node, out: Arc<Mutex<(Option<Error>, usize)>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let full = fill(&config.pool);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        let (read, freed) = {
            let mut recv = pin!(receiver.recv());
            while transport.status().waited == Span::ZERO {
                let read = poll_once(recv.as_mut()).await;
                assert!(read.is_pending(), "no block: {read:?}");
                clock.sleep(Span::MILLISECOND).await;
            }
            clock.sleep(READ).await;
            let before = ALLOCATOR.held();
            let read = poll_once(recv.as_mut()).await;
            (read, before.saturating_sub(ALLOCATOR.held()))
        };
        let error = match read {
            Poll::Ready(Err(error)) => Some(error),
            _ => None,
        };
        *out.lock().expect("not poisoned") = (error, freed);
        drop((receiver, full));
    });
    drop(started.expect("a shard"));
}

/// Polls `future` once.
async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// Takes every block of `pool` that holds a message of [`LEN`] bytes.
fn fill(pool: &Pool) -> Vec<Unique> {
    let mut full = Vec::new();
    for len in [pool.largest(), LEN] {
        while let Ok(block) = pool.alloc(len) {
            full.push(block);
        }
    }
    full
}

fn filled(pool: &Pool, bytes: usize) -> Block {
    let mut block = pool.alloc(bytes).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let pool = block::Config { budget: 1 << 20 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(MESSAGE_BYTES_MAX).expect("not zero"),
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
