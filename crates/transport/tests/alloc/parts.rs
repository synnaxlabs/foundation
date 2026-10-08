//! A `send_parts` allocates as a `send` of one block of the same bytes, plus one
//! allocation for the copy of each stretch over 1452 bytes, one when noq-proto first
//! takes such a copy in part, and one for each growth of the segment queue of
//! noq-proto.

use std::iter;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::{Block, Heap, Pool};
use env::clock::Clock;
use sim::Sim;
use sim::node::Node;
use transport::stream::{Part, Sender};
use transport::{Address, Class, Code, Config, Error, Session, Transport};
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{CLIENT, PORT, SERVER, filled, part};

/// Long enough for the peer to read and acknowledge a message.
const PAUSE: Span = Span::from_nanos(100_000_000);
const CLOSED: Error = Error::PeerClosed { code: Code(0) };
/// The sends of each kind before the counted ones.
const WARMUP: usize = 8;
const ROUNDS: usize = 8;

/// The parts of a message, and the allocations of its `send_parts` over those of a
/// `send`, with their cause.
struct Shape {
    name: &'static str,
    parts: Vec<Part>,
    over: u64,
    cause: &'static str,
}

/// The shapes that [`measure`] sends. The third one is many short ranges.
fn shapes() -> [Shape; 6] {
    [
        Shape {
            name: "3 ranges of 8 KiB",
            parts: spaced(3, 8 << 10, (8 << 10) + 8),
            over: 0,
            cause: "none: the segment queue holds the header and 3 slices",
        },
        // noq-proto shrinks its segment queue as the peer acknowledges it, so each
        // message grows it again.
        Shape {
            name: "8 ranges of 8 KiB",
            parts: spaced(8, 8 << 10, (8 << 10) + 8),
            over: 2,
            cause: "the segment queue grows to 8, then 16: the header and 8 slices",
        },
        Shape {
            name: "1000 ranges of 8 B",
            parts: spaced(1000, 8, 16),
            over: 1,
            cause: "the copy of the stretch",
        },
        Shape {
            name: "a stretch of 1452 bytes with zeros",
            parts: vec![series(0..1000, 2), series(2000..2450, 0)],
            over: 0,
            cause: "none: noq-proto copies it from the buffer of the connection",
        },
        Shape {
            name: "a range of 1452 bytes, then 1 byte and a zero",
            parts: vec![series(0..1452, 0), series(2000..2001, 1)],
            over: 1,
            cause: "the copy of the stretch of 1454 bytes",
        },
        Shape {
            name: "two adjacent ranges of 1000 bytes",
            parts: vec![series(0..1000, 0), series(1000..2000, 0)],
            over: 0,
            cause: "none: one run of 2000 bytes goes as a slice",
        },
    ]
}

/// A part: the series of one channel.
fn series(range: Range<usize>, zeros: u8) -> Part {
    Part { range, zeros }
}

/// `ranges` ranges of `len` bytes, `stride` bytes apart.
fn spaced(ranges: usize, len: usize, stride: usize) -> Vec<Part> {
    (0..ranges)
        .map(|at| series(at * stride..at * stride + len, 0))
        .collect()
}

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
    /// The bytes of the message.
    fn bytes(&self) -> usize {
        let size = |part: &Part| part.range.len() + usize::from(part.zeros);
        self.parts.iter().map(size).sum()
    }

    /// The bytes of the block that the parts are ranges of.
    fn large(&self) -> usize {
        self.parts
            .iter()
            .map(|part| part.range.end)
            .max()
            .unwrap_or(0)
    }
}

pub(crate) fn main() {
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
        let shapes = shapes();
        for shape in &shapes {
            measure(&mut sender, &pool, &clock, shape).await;
        }
        sender.finish().expect("finished");
        // A stream that kept a copy of the parts of each message would grow its list
        // at the first message of many parts.
        let short = &shapes[2];
        let mut first = [0; 2];
        for (count, parts) in first.iter_mut().zip([&[][..], &short.parts]) {
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
    // A stretch of 100 bytes, then a run of 4000 bytes, on a new connection: the
    // stretch buffer grows only with the walk. In one part, one allocation of 100
    // bytes. In parts of 8 bytes, 100, then each doubling to 1600.
    let adjacent = (0..500).map(|at| series(200 + at * 8..208 + at * 8, 0));
    let walks = [
        (vec![series(0..100, 0), series(200..4_200, 0)], 1),
        (iter::once(series(0..100, 0)).chain(adjacent).collect(), 5),
    ];
    for (parts, over) in walks {
        run(WIDE, async move |session, pool, clock| {
            let opened = session.open_sender(Class::Complete).await;
            let mut sender = opened.expect("a stream");
            clock.sleep(PAUSE).await;
            poll(&mut sender, filled(&pool, 8), &[]);
            clock.sleep(PAUSE).await;
            let block = filled(&pool, 4_100);
            let sent = ALLOCATOR.count(|| poll(&mut sender, block, &[])).1;
            clock.sleep(PAUSE).await;
            let block = filled(&pool, 4_200);
            let parted = ALLOCATOR.count(|| poll(&mut sender, block, &parts)).1;
            sender.finish().expect("finished");
            assert_eq!(parted, sent + over, "the growths of the stretch buffer");
        });
    }
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
            .dial(SERVER.public(), &[Address::Udp(at)])
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
    let (bytes, large) = (shape.bytes(), shape.large());
    for round in 0..WARMUP + ROUNDS {
        clock.sleep(PAUSE).await;
        let block = filled(pool, bytes);
        let sent = ALLOCATOR.count(|| poll(sender, block, &[])).1;
        clock.sleep(PAUSE).await;
        let block = filled(pool, large);
        let parted = ALLOCATOR.count(|| poll(sender, block, &shape.parts)).1;
        if round >= WARMUP {
            assert_eq!(
                parted,
                sent + shape.over,
                "{}, over send: {}",
                shape.name,
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
    let parts = spaced(NARROW.message / 8, 8, 16);
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
                "a stretch that the window takes in parts, over send: the copy of \
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

/// [`crate::common::config`] with `limits` and a pool of 4 MiB.
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
        ..crate::common::config(node, tasks, key)
    }
}
