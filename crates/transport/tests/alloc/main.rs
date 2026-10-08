//! A `send_parts` allocates as a `send` of one block of the same bytes, plus one
//! allocation for the buffer of each stretch over 1452 bytes, one when noq-proto first
//! takes such a buffer in part, and one for each growth of the segment queue of
//! noq-proto. This binary has no test harness: the count covers each thread, and a
//! harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::{Block, Heap, Pool};
use common::{CLIENT, PORT, SERVER, filled, part, public};
use env::clock::Clock;
use sim::Sim;
use sim::node::Node;
use transport::stream::{Part, Sender};
use transport::{Address, Class, Code, Config, Error, Session, Transport};
use types::node::PrivateKey;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

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

const SHAPES: [Shape; 3] = [
    Shape {
        ranges: 3,
        len: 8 << 10,
        stride: (8 << 10) + 8,
        over: 0,
        cause: "none: the segment queue holds the header and 3 slices",
    },
    // noq-proto shrinks its segment queue as the peer acknowledges it, so each
    // message grows it again.
    Shape {
        ranges: 8,
        len: 8 << 10,
        stride: (8 << 10) + 8,
        over: 2,
        cause: "the segment queue grows to 8, then 16: the header and 8 slices",
    },
    Shape {
        ranges: 1000,
        len: 8,
        stride: 16,
        over: 1,
        cause: "the buffer of the stretch",
    },
];

/// The limits of a transport: its largest message and its window.
#[derive(Clone, Copy)]
struct Limits {
    message: usize,
    window: usize,
}

const WIDE: Limits = Limits {
    message: 1 << 17,
    window: 1 << 20,
};
/// The window takes a message of the largest size only in part, after its header.
const NARROW: Limits = Limits {
    message: 16_000,
    window: 16_000,
};

impl Shape {
    fn parts(&self) -> Vec<Part> {
        (0..self.ranges)
            .map(|at| Part {
                range: at * self.stride..at * self.stride + self.len,
                zeros: 0,
            })
            .collect()
    }

    /// The bytes of the block that the parts are ranges of.
    fn large(&self) -> usize {
        self.ranges * self.stride
    }
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    run(WIDE, async |session, pool, clock| {
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        for shape in &SHAPES {
            measure(&mut sender, &pool, &clock, shape).await;
        }
        sender.finish().expect("finished");
        // A stream that kept a copy of the parts of each message would grow its list
        // at the first message of many parts.
        let short = &SHAPES[2];
        let mut first = [0; 2];
        for (count, parts) in first.iter_mut().zip([&[][..], &short.parts()]) {
            let opened = session.open_sender(Class::Complete).await;
            let mut sender = opened.expect("a stream");
            clock.sleep(PAUSE).await;
            poll(&mut sender, filled(&pool, 8), &[]);
            clock.sleep(PAUSE).await;
            let block = filled(&pool, short.large());
            *count = ALLOCATOR.count(|| poll(&mut sender, block, parts)).1;
            sender.finish().expect("finished");
        }
        let [sent, parted] = first;
        assert_eq!(
            parted,
            sent + short.over,
            "the first message of parts on a stream: {}",
            short.cause
        );
    });
    run(NARROW, async |session, pool, clock| {
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        cut(&mut sender, &pool, &clock).await;
        sender.finish().expect("finished");
    });
}

/// Runs `client` on a session of a client and a server with `limits`, then closes the
/// session.
fn run(
    limits: Limits,
    client: impl AsyncFnOnce(Session, Rc<Pool>, Clock) + Send + 'static,
) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    serve(&server, limits);
    sim.run_on(&node, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT, limits);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(at)])
            .await
            .expect("a session");
        let clock = node.clock();
        client(session.clone(), pool, clock.clone()).await;
        clock.sleep(PAUSE).await;
        session.close(Code(0));
        clock.sleep(Span::MILLISECOND).await;
    })
    .expect("the test runs");
}

/// Sends messages of `shape` and of a single block of the same bytes, and checks the
/// allocations of each after warm-up.
///
/// # Panics
///
/// When a count is not that of `shape`.
async fn measure(sender: &mut Sender, pool: &Pool, clock: &Clock, shape: &Shape) {
    let parts = shape.parts();
    let bytes = shape.ranges * shape.len;
    let large = shape.large();
    for round in 0..WARMUP + ROUNDS {
        clock.sleep(PAUSE).await;
        let block = filled(pool, bytes);
        let sent = ALLOCATOR.count(|| poll(sender, block, &[])).1;
        clock.sleep(PAUSE).await;
        let block = filled(pool, large);
        let parted = ALLOCATOR.count(|| poll(sender, block, &parts)).1;
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

/// Sends messages of one stretch of the largest size and of a single block of the same
/// bytes, each after a message of half that size, so that the window takes each in
/// parts. Checks the allocations of the first write of each, which noq-proto takes in
/// part, and of the writes that follow it.
///
/// # Panics
///
/// When the first write of the stretch does not allocate as that of the block plus
/// the buffer of the stretch and the shared count that `bytes` makes when noq-proto
/// first takes the buffer in part, or the later writes do not allocate as those of
/// the block.
async fn cut(sender: &mut Sender, pool: &Pool, clock: &Clock) {
    let parts: Vec<Part> = (0..NARROW.message / 8)
        .map(|at| Part {
            range: at * 16..at * 16 + 8,
            zeros: 0,
        })
        .collect();
    let half = NARROW.message / 2;
    for round in 0..WARMUP + ROUNDS {
        clock.sleep(PAUSE).await;
        poll(sender, filled(pool, half), &[]);
        let block = filled(pool, NARROW.message);
        let sent = cut_send(clock, pin!(sender.send(block))).await;
        clock.sleep(PAUSE).await;
        poll(sender, filled(pool, half), &[]);
        let block = filled(pool, 2 * NARROW.message);
        let parted = cut_send(clock, pin!(sender.send_parts(block, &parts))).await;
        if round >= WARMUP {
            assert_eq!(
                parted,
                [sent[0] + 2, sent[1]],
                "a stretch that the window takes in parts, over send: the buffer of \
                 the stretch and the shared count of bytes at the first cut, then no \
                 copy of the rest"
            );
        }
    }
}

/// Polls `send` until it ends, with a pause before each poll after the first, and
/// gives the allocations of the first poll and of the polls after it.
///
/// # Panics
///
/// When the first poll ends the send, or the send fails.
async fn cut_send(
    clock: &Clock,
    mut send: Pin<&mut impl Future<Output = Result<(), Error>>>,
) -> [u64; 2] {
    let mut cx = Context::from_waker(Waker::noop());
    let (polled, first) = ALLOCATOR.count(|| send.as_mut().poll(&mut cx));
    assert!(
        polled.is_pending(),
        "the window takes the first write in part"
    );
    let mut after = 0;
    loop {
        clock.sleep(PAUSE).await;
        let (polled, count) = ALLOCATOR.count(|| send.as_mut().poll(&mut cx));
        after += count;
        if let Poll::Ready(sent) = polled {
            sent.expect("the send goes");
            return [first, after];
        }
    }
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

fn serve(node: &Node, limits: Limits) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks.clone(), SERVER, limits);
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
        assert_eq!(session.closed().await, CLOSED, "the client closes");
    });
    drop(started.expect("a shard"));
}

/// [`common::config`] with `limits` and a pool of 4 MiB.
fn config(
    node: &Node,
    tasks: env::tasks::Tasks,
    key: PrivateKey,
    limits: Limits,
) -> Config {
    let pool = block::Config { budget: 1 << 22 };
    let memory = Heap::new(pool.reservation());
    Config {
        message_bytes_max: NonZeroUsize::new(limits.message).expect("not zero"),
        window_bytes: limits.window,
        pool: Rc::new(Pool::new(pool, memory)),
        ..common::config(node, tasks, key)
    }
}
