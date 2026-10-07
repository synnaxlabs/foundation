//! The cost of a `stream::Sender::send` that is ready on its first poll, on a session
//! between two sim nodes. Run with `cargo bench -p transport --bench send`.
//!
//! Each round fills its blocks, then times one poll of each send: a burst of 64 sends
//! into streams the peer has drained. A sim sleep between rounds lets the peer read
//! and acknowledge. A send that waits on its first poll panics, so no number holds a
//! wait. A figure is per send over a round, and some sends in a burst cost more than
//! others.
//!
//! The last line has a `Latest` and a `Complete` stream send at once, over a session
//! whose peer window holds a quarter of a round, so the sends wait for room and for
//! their turn. It times each poll of each send, not the sim or the peer between polls.
//! A round in which the two classes never wait at once panics, so the line always
//! measures classes that compete.
//!
//! A send reads the clock and wakes a task, so the control does both per block. The sim
//! and `os` costs for both differ: on a Xeon 8488C, an `os` clock read costs about 7
//! times a sim one, and an `os` wake about a quarter of a sim one. Compare a send with
//! the control, or a build with another build, not with an `os` number. To compare two
//! builds, run each several times in turn on one pinned core and compare p10 and p50:
//! on a busy machine, p90 holds the preemptions.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::RefCell;
use std::future;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::stream::Sender;
use transport::{Address, Class, Code, Config, Error, Port, Transport};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CLIENT: PrivateKey = PrivateKey([1; 32]);
const SERVER: PrivateKey = PrivateKey([2; 32]);
const NARROW: PrivateKey = PrivateKey([3; 32]);
const PORT: u16 = 4433;
/// The room of the server for the scenarios that do not wait: a round of the
/// largest message fits.
const WIDE: Room = Room {
    window_bytes: 1 << 22,
    message_bytes_max: 1 << 16,
};
/// The room of the server for the competing scenario: a quarter of a round.
const TIGHT: Room = Room {
    window_bytes: 16 << 10,
    message_bytes_max: 4 << 10,
};
/// The name and message size of the competing scenario.
const COMPETE: (&str, usize) = ("latest and complete 1 KiB", 1024);

/// Sends per round. A round fits the `WIDE` window, so no send waits there.
const SENDS: usize = 64;
/// Rounds per scenario before the timed rounds.
const WARMUP: usize = 50;
/// Timed rounds per scenario.
const ROUNDS: usize = 500;
/// The sim time between rounds, in which the peer reads and acknowledges a round.
const PAUSE: Span = Span::from_nanos(100_000_000);
/// How the session ends.
const CLOSED: Error = Error::PeerClosed { code: Code(0) };

/// The flow control room a node gives its peer.
#[derive(Clone, Copy)]
struct Room {
    window_bytes: usize,
    message_bytes_max: usize,
}

/// What one scenario sends.
struct Scenario {
    name: &'static str,
    load: Load,
    bytes: usize,
}

enum Load {
    /// No send: the same loop polls a ready future, reads the clock, and wakes the
    /// task, for the floor of the harness and of the sim.
    Control,
    /// One stream per class, sent on in turn.
    Streams(&'static [Class]),
}

const SCENARIOS: [Scenario; 5] = [
    Scenario {
        name: "control 1 KiB",
        load: Load::Control,
        bytes: 1024,
    },
    Scenario {
        name: "complete 64 B",
        load: Load::Streams(&[Class::Complete]),
        bytes: 64,
    },
    Scenario {
        name: "complete 1 KiB",
        load: Load::Streams(&[Class::Complete]),
        bytes: 1024,
    },
    Scenario {
        name: "complete 16 KiB",
        load: Load::Streams(&[Class::Complete]),
        bytes: 16 * 1024,
    },
    Scenario {
        name: "8 complete 1 KiB",
        load: Load::Streams(&[Class::Complete; 8]),
        bytes: 1024,
    },
];

/// The result of one scenario.
struct Measured {
    name: &'static str,
    /// The nanoseconds of each timed round, sorted.
    nanos: Vec<u64>,
    allocations: u64,
}

fn main() {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let narrow = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let narrow_at = SocketAddr::new(narrow.addresses()[0], PORT);
    serve(&server, SERVER, WIDE);
    serve(&narrow, NARROW, TIGHT);
    let lines = sim
        .run_on(&client, move |node, tasks| async move {
            let config = config(&node, tasks, CLIENT, WIDE);
            let pool = Rc::clone(&config.pool);
            let transport =
                Transport::new(config, part(&node, 0)).expect("a transport");
            let session = transport
                .dial(public(&SERVER), &[Address::Udp(at)])
                .await
                .expect("a session");
            let clock = node.clock();
            let mut lines = Vec::with_capacity(SCENARIOS.len());
            for scenario in &SCENARIOS {
                let classes = match scenario.load {
                    Load::Control => &[][..],
                    Load::Streams(classes) => classes,
                };
                let mut senders = Vec::with_capacity(classes.len());
                for &class in classes {
                    senders.push(session.open_sender(class).await.expect("a stream"));
                }
                lines.push(measure(&clock, &pool, &mut senders, scenario).await);
                for sender in &mut senders {
                    sender.finish().expect("the stream finishes");
                }
            }
            session.close(Code(0));
            let session = transport
                .dial(public(&NARROW), &[Address::Udp(narrow_at)])
                .await
                .expect("a session");
            let latest = session.open_sender(Class::Latest).await;
            let complete = session.open_sender(Class::Complete).await;
            let senders = [latest, complete].map(|sender| sender.expect("a stream"));
            lines.push(compete(&pool, senders).await);
            session.close(Code(0));
            // A shard that ends drops its tasks, so give the close time to go out.
            clock.sleep(Span::MILLISECOND).await;
            lines
        })
        .expect("the bench runs");
    print(&lines);
}

/// Starts a shard on `node` that accepts one session as `key` with `room`, and reads
/// each stream until the session closes.
///
/// # Panics
///
/// On an error other than the client's close with code 0.
fn serve(node: &Node, key: PrivateKey, room: Room) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks.clone(), key, room);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        loop {
            let mut receiver = match session.accept().await {
                Ok(incoming) => incoming.receiver,
                Err(CLOSED) => return,
                Err(error) => panic!("the server failed to accept: {error}"),
            };
            tasks.spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(Some(_)) => {}
                        Ok(None) | Err(CLOSED) => return,
                        Err(error) => panic!("the server failed to read: {error}"),
                    }
                }
            });
        }
    });
    drop(started.expect("a shard"));
}

/// Runs the rounds of `scenario` on `senders`, empty for the control.
async fn measure(
    clock: &env::clock::Clock,
    pool: &Pool,
    senders: &mut [Sender],
    scenario: &Scenario,
) -> Measured {
    let waker = future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
    let mut nanos = Vec::with_capacity(ROUNDS);
    let mut allocations = 0;
    let mut held = Vec::with_capacity(SENDS);
    for round in 0..WARMUP + ROUNDS {
        clock.sleep(PAUSE).await;
        let blocks: Vec<Block> =
            (0..SENDS).map(|_| filled(pool, scenario.bytes)).collect();
        let (span, counted) = ALLOCATOR.count(|| match scenario.load {
            Load::Control => poll_control(clock, &waker, blocks, &mut held),
            Load::Streams(_) => poll_sends(senders, blocks),
        });
        held.clear();
        if round >= WARMUP {
            nanos.push(span);
            allocations += counted;
        }
    }
    nanos.sort_unstable();
    Measured {
        name: scenario.name,
        nanos,
        allocations,
    }
}

/// Polls one send of each block once, on `senders` in turn, and gives the nanoseconds
/// it took.
///
/// # Panics
///
/// When a send waits or fails.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn poll_sends(senders: &mut [Sender], blocks: Vec<Block>) -> u64 {
    let mut cx = Context::from_waker(Waker::noop());
    let mut blocks = blocks.into_iter();
    let start = Instant::now();
    while blocks.len() > 0 {
        for (sender, block) in senders.iter_mut().zip(&mut blocks) {
            match pin!(sender.send(block)).poll(&mut cx) {
                Poll::Ready(sent) => sent.expect("the send goes"),
                Poll::Pending => panic!("a send waited on its first poll: raise PAUSE"),
            }
        }
    }
    nanos(Instant::now().duration_since(start))
}

/// The loop of [`poll_sends`] with no send: per block, it polls a ready future,
/// reads `clock`, wakes `waker`, and moves the block into `held`, so its drop is not
/// timed. Gives the nanoseconds it took.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn poll_control(
    clock: &env::clock::Clock,
    waker: &Waker,
    blocks: Vec<Block>,
    held: &mut Vec<Block>,
) -> u64 {
    let mut cx = Context::from_waker(Waker::noop());
    let start = Instant::now();
    for block in blocks {
        match pin!(future::ready(Ok::<_, Error>(block))).poll(&mut cx) {
            Poll::Ready(block) => held.push(block.expect("a ready future")),
            Poll::Pending => panic!("a ready future is ready"),
        }
        std::hint::black_box(clock.now());
        waker.wake_by_ref();
    }
    nanos(Instant::now().duration_since(start))
}

/// Runs the rounds of the competing scenario. Per round, `senders`, a `Latest` and
/// a `Complete` stream, each send half of [`SENDS`] blocks at once.
///
/// # Panics
///
/// When a send fails, or after a round in which no send of one class waited while a
/// send of the other did.
async fn compete(pool: &Pool, senders: [Sender; 2]) -> Measured {
    let (name, bytes) = COMPETE;
    let [mut latest, mut complete] = senders;
    let mut nanos = Vec::with_capacity(ROUNDS);
    let mut allocations = 0;
    for round in 0..WARMUP + ROUNDS {
        let tally = RefCell::new(Tally::default());
        let blocks = || (0..SENDS / 2).map(|_| filled(pool, bytes)).collect();
        let latest = send_each(&mut latest, blocks(), 0, &tally);
        let complete = send_each(&mut complete, blocks(), 1, &tally);
        join(latest, complete).await;
        let tally = tally.into_inner();
        assert!(
            tally.competed,
            "the classes did not compete in round {round}"
        );
        if round >= WARMUP {
            nanos.push(tally.nanos);
            allocations += tally.allocations;
        }
    }
    for sender in [&mut latest, &mut complete] {
        sender.finish().expect("the stream finishes");
    }
    nanos.sort_unstable();
    Measured {
        name,
        nanos,
        allocations,
    }
}

/// Sends each of `blocks` on `sender` in order, and adds each poll of a send to
/// `tally` as class `class`.
async fn send_each(
    sender: &mut Sender,
    blocks: Vec<Block>,
    class: usize,
    tally: &RefCell<Tally>,
) {
    for block in blocks {
        let mut send = pin!(sender.send(block));
        future::poll_fn(|cx| tally.borrow_mut().poll(class, send.as_mut(), cx))
            .await
            .expect("the send goes");
    }
}

/// The polls of the sends of one competing round.
#[derive(Default)]
struct Tally {
    nanos: u64,
    allocations: u64,
    /// Whether the last poll of the current send of each class waited.
    waiting: [bool; 2],
    /// Whether a send of each class waited at once.
    competed: bool,
}

impl Tally {
    /// Polls `send` of class `class` once, and adds its time and allocations.
    #[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
    fn poll<F: Future>(
        &mut self,
        class: usize,
        send: Pin<&mut F>,
        cx: &mut Context<'_>,
    ) -> Poll<F::Output> {
        let start = Instant::now();
        let (polled, counted) = ALLOCATOR.count(|| send.poll(cx));
        self.nanos += nanos(Instant::now().duration_since(start));
        self.allocations += counted;
        self.waiting[class] = polled.is_pending();
        self.competed |= self.waiting == [true, true];
        polled
    }
}

/// Runs `a` and `b` at once. Each is polled only when it was woken, so no poll of a
/// send is spurious.
async fn join(a: impl Future<Output = ()>, b: impl Future<Output = ()>) {
    let mut futures: [Pin<&mut dyn Future<Output = ()>>; 2] = [pin!(a), pin!(b)];
    let parent = future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
    let flags = [(); 2].map(|()| {
        Arc::new(Flag {
            woken: AtomicBool::new(true),
            parent: parent.clone(),
        })
    });
    let wakers = flags.clone().map(Waker::from);
    let mut done = [false; 2];
    future::poll_fn(|cx| {
        assert!(
            cx.waker().will_wake(&parent),
            "the task's waker is the same"
        );
        for at in 0..2 {
            if !done[at] && flags[at].woken.swap(false, Ordering::Relaxed) {
                let mut cx = Context::from_waker(&wakers[at]);
                done[at] = futures[at].as_mut().poll(&mut cx).is_ready();
            }
        }
        if done == [true; 2] {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// The waker of one future in [`join`]: marks it woken and wakes the task.
struct Flag {
    woken: AtomicBool,
    parent: Waker,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Relaxed);
        self.parent.wake_by_ref();
    }
}

fn nanos(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).expect("a round takes under 2^64 ns")
}

/// A block from `pool` of `bytes` bytes.
fn filled(pool: &Pool, bytes: usize) -> Block {
    let mut block = pool.alloc(bytes).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}

/// A config for `key` on `node` that gives its peer `room`.
fn config(
    node: &Node,
    tasks: env::tasks::Tasks,
    key: PrivateKey,
    room: Room,
) -> Config {
    let pool = block::Config { budget: 1 << 24 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(room.message_bytes_max).expect("not zero"),
        window_bytes: room.window_bytes,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(10 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool: Rc::new(Pool::new(pool, memory)),
    }
}

/// The one part of a port bound at `port` on the first IP of `node`.
fn part(node: &Node, port: u16) -> transport::port::Part {
    let at = SocketAddr::new(node.addresses()[0], port);
    let port = Port::bind(&node.net(), at).expect("a port");
    port.split(NonZeroUsize::MIN).pop().expect("one part")
}

/// The public key of `key`.
fn public(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("32 bytes");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("aws-lc makes no key of small order")
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Measured]) {
    println!("ns per send over {ROUNDS} rounds of {SENDS} sends");
    println!("pN: the round at percentile N, over its sends");
    println!(
        "{:<26} {:>9} {:>9} {:>9} {:>12}",
        "scenario", "p10", "p50", "p90", "allocs/send"
    );
    for line in lines {
        let at = |percent: usize| per(line.nanos[ROUNDS * percent / 100]) / per(SENDS);
        let allocations = per(line.allocations) / per(ROUNDS * SENDS);
        println!(
            "{:<26} {:>9.1} {:>9.1} {:>9.1} {allocations:>12.2}",
            line.name,
            at(10),
            at(50),
            at(90),
        );
    }
}

/// `value` as a float, exact below 2^53.
#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "a printed figure loses no digit it shows"
)]
fn per(value: impl TryInto<u64, Error: std::fmt::Debug>) -> f64 {
    value.try_into().expect("fits 64 bits") as f64
}
