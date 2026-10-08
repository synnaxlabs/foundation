//! A message that waits for a block from the pool is held on the heap, outside the
//! pool, in one buffer made at its length. When its stream resets or its session
//! closes, the read that gives the error frees it, though the caller keeps the
//! receiver. The receiver then keeps a list of at most 64 chunks, not one sized by the
//! message. The count covers each thread, so this binary has no test harness. The
//! sim runs on one thread, so the count is exact.

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
/// A message that the read takes over many polls.
const LEN: usize = 60_000;
/// A message longer than 64 packets of 1472 bytes, so that a read of it in one poll
/// copies a full list of chunks.
const LONG: usize = 100_000;
/// A message longer than 128 packets of 1472 bytes, so that the read copies a full
/// list twice before it holds the rest.
const LONGER: usize = 250_000;
const MESSAGE_BYTES_MAX: usize = 1 << 18;
/// The heap of a list of 64 chunks, since each slot is 32 bytes.
const LIST: usize = 2 << 10;
/// The heap of the cell that holds a closed session's error, which the drop of its
/// receiver frees.
const CLOSED: usize = 72;
/// When the server first polls a long message, once it is whole and before the end.
const WHOLE: Span = Span::from_nanos(250_000_000);
/// When the client ends the stream or the session, after its send.
const END: Span = Span::from_nanos(500_000_000);
/// When the server reads the end, after the message waits for a block.
const READ: Span = Span::from_nanos(1_000_000_000);

/// The error of the server's read after the end, the heap bytes that read freed, and
/// the net heap bytes that the drop of the receiver then gives back.
type Out = (Option<Error>, usize, usize);

/// How the client ends the message that waits.
#[derive(Clone, Copy, Debug)]
enum End {
    /// It drops its sender, which resets the stream with code 0.
    Reset,
    /// It closes the session with code 7.
    Close,
}

fn main() {
    let reset = Error::Reset { code: Code(0) };
    for (end, len, first, error) in [
        (End::Reset, LEN, Span::ZERO, reset.clone()),
        (
            End::Close,
            LEN,
            Span::ZERO,
            Error::PeerClosed { code: Code(7) },
        ),
        (End::Reset, LONG, WHOLE, reset.clone()),
        (End::Reset, LONGER, WHOLE, reset),
    ] {
        let (read, freed, kept) = run(end, len, first);
        assert_eq!(
            read,
            Some(error),
            "{end:?}, {len} bytes: the read after the end"
        );
        assert_eq!(
            freed, len,
            "{end:?}, {len} bytes: the read that gives the error frees the buffer of \
             the message, made at its length"
        );
        let kept_max = match end {
            End::Reset => LIST,
            End::Close => LIST + CLOSED,
        };
        assert!(
            kept <= kept_max,
            "{end:?}, {len} bytes: the receiver keeps {kept} bytes after the error"
        );
    }
}

/// The [`Out`] of the server's read after the client sends a message of `len` bytes
/// and ends the stream in the way of `end`. The server first polls `first` after the
/// stream comes.
fn run(end: End, len: usize, first: Span) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0, 0)));
    serve(&server, len, first, Arc::clone(&out));
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
        sender.send(filled(&pool, len)).await.expect("sent");
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
    out.lock().expect("not poisoned").clone()
}

/// Starts the server on `node`. It fills its pool for messages of `len` bytes, first
/// polls `first` after the stream comes, lets the client's message wait for a block,
/// and puts the [`Out`] of its read after the end in `out`.
fn serve(node: &Node, len: usize, first: Span, out: Arc<Mutex<Out>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let full = fill(&config.pool, len);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        clock.sleep(first).await;
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
        let before = ALLOCATOR.held();
        drop(receiver);
        let kept = before.saturating_sub(ALLOCATOR.held());
        *out.lock().expect("not poisoned") = (error, freed, kept);
        drop(full);
    });
    drop(started.expect("a shard"));
}

/// Polls `future` once.
async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// Takes every block of `pool` that holds a message of `len` bytes.
fn fill(pool: &Pool, len: usize) -> Vec<Unique> {
    let mut full = Vec::new();
    for len in [pool.largest(), len] {
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
