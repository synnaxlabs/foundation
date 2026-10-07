//! The cost of a `stream::Sender::send`, and of a `try_send`, on a session between two
//! sim nodes. Run with `cargo bench -p transport --bench send`. A figure is per timed
//! send over a round, and some sends in a round cost more than others.
//!
//! The `SCENARIOS` lines time a send that is ready on its first poll, and the `try`
//! lines a `try_send` that takes its message. Each round fills its blocks, then times
//! one poll of each send, or one `try_send` of each block: a burst of 64 sends into
//! streams the peer has drained. A sim sleep before each round lets the peer read and
//! acknowledge. A send that waits on its first poll, or a `try_send` that gives its
//! message back, panics, so no number holds a wait. A scenario times `send` and
//! `try_send` in alternating rounds on the same streams. The cost of a line moves with
//! its place in the run, so compare a `try` line only with the line of its own
//! scenario. A `try_send` makes no poll.
//!
//! The `waiting` lines send a round on their streams at once, over a session whose
//! peer window holds a quarter of a round, so the sends wait for the QUIC window and
//! for their turn. They time the polls of a send, not the sim or the peer between
//! polls. `latest and complete 1 KiB waiting` times a send only when it ends while a
//! send of the other class waits, so a class that sends alone adds nothing; `timed`
//! gives the share of sends it timed. In each round, a send of each class must end
//! while the other class waits, and the round must time at least half its sends so
//! that its figure stands on enough sends. Over the timed rounds, the `Complete`
//! sends it timed must be at least twice its `Latest` sends: the share gives
//! `Complete` 3 bytes for each byte of `Latest`, and both classes send messages of
//! one size. After the rounds, the server must have
//! one stream of each class. If not, the bench panics. Its control is `complete 1 KiB
//! waiting`, whose send must wait in each round. Each poll has a timing cost, so
//! compare the two lines with their polls per send.
//!
//! A send reads the clock and wakes a task, so the control does both per block. The sim
//! and `os` costs for both differ: on a Xeon 8488C, an `os` clock read costs about 7
//! times a sim one, and an `os` wake about a quarter of a sim one. Compare a send with
//! the control, or a build with another build, not with an `os` number. To compare two
//! builds, run each several times in turn on one pinned core and compare p10 and p50:
//! on a busy machine, p90 holds the preemptions. Drop a run whose control reads above
//! its usual value: a busy sibling core makes every line cost up to twice as much.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::RefCell;
use std::future;
use std::mem;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::Sim;
use sim::node::Node;
use transport::stream::Sender;
use transport::{Address, Class, Code, Config, Error, Port, Session, Transport};
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
/// The room of the server for the `WAITING` scenarios: a quarter of a round.
const TIGHT: Room = Room {
    window_bytes: 16 << 10,
    message_bytes_max: 4 << 10,
};

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
#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    load: Load,
    bytes: usize,
}

#[derive(Clone, Copy)]
enum Load {
    /// No send: the same loop polls a ready future, reads the clock, and wakes the
    /// task, for the floor of the harness and of the sim.
    Control,
    /// One stream per class, sent on in turn.
    Streams(&'static [Class]),
}

/// How a round of [`measure`] sends its blocks.
#[derive(Clone, Copy)]
enum Call {
    /// The loop of [`Load::Control`].
    Control,
    Send,
    TrySend,
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

/// The scenarios whose sends wait, on a session with the room `TIGHT`, and what each
/// round of each must show.
const WAITING: [(Scenario, Premise); 2] = [
    (
        Scenario {
            name: "complete 1 KiB waiting",
            load: Load::Streams(&[Class::Complete]),
            bytes: 1024,
        },
        Premise::Waits,
    ),
    (
        Scenario {
            name: "latest and complete 1 KiB waiting",
            load: Load::Streams(&[Class::Latest, Class::Complete]),
            bytes: 1024,
        },
        Premise::Competes,
    ),
];

/// What each round of a `WAITING` scenario must show, or the bench panics.
#[derive(Clone, Copy, Debug)]
enum Premise {
    /// A send waits.
    Waits,
    /// A `Latest` send and a `Complete` send each end while a send of the other
    /// class waits. Over the timed rounds, the timed `Complete` sends must also be at
    /// least twice the timed `Latest` sends.
    Competes,
}

/// The class of each stream a server accepted, in the order it accepted them.
type Accepted = Arc<Mutex<Vec<Class>>>;

/// The result of one scenario.
struct Measured {
    name: String,
    /// The nanoseconds per timed send of each round, sorted.
    nanos: Vec<f64>,
    allocations: u64,
    polls: u64,
    /// The timed sends of all rounds.
    sends: u64,
}

fn main() {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let narrow = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let narrow_at = SocketAddr::new(narrow.addresses()[0], PORT);
    serve(&server, SERVER, WIDE);
    let accepted = serve(&narrow, NARROW, TIGHT);
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
                let mut senders = open(&session, scenario.load).await;
                lines.extend(measure(&clock, &pool, &mut senders, scenario).await);
                for sender in &mut senders {
                    sender.finish().expect("the stream finishes");
                }
            }
            session.close(Code(0));
            let session = transport
                .dial(public(&NARROW), &[Address::Udp(narrow_at)])
                .await
                .expect("a session");
            for (scenario, premise) in WAITING {
                let mut senders = open(&session, scenario.load).await;
                let line = compete(&pool, &mut senders, scenario, premise, &accepted);
                lines.push(line.await);
                for sender in &mut senders {
                    sender.finish().expect("the stream finishes");
                }
            }
            session.close(Code(0));
            // A shard that ends drops its tasks, so give the close time to go out.
            clock.sleep(Span::MILLISECOND).await;
            lines
        })
        .expect("the bench runs");
    print(&lines);
}

/// Starts a shard on `node` that accepts one session as `key` with `room`, and reads
/// each stream until the session closes. Returns the class of each stream it accepts.
///
/// # Panics
///
/// On an error other than the client's close with code 0.
fn serve(node: &Node, key: PrivateKey, room: Room) -> Accepted {
    let accepted = Accepted::default();
    let classes = Arc::clone(&accepted);
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
                Ok(incoming) => {
                    classes.lock().expect("not poisoned").push(incoming.class);
                    incoming.receiver
                }
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
    accepted
}

/// Runs the rounds of `scenario` on `senders`, empty for the control. Gives the
/// control's line, or the lines of `send` and `try_send`, timed in alternating rounds.
async fn measure(
    clock: &env::clock::Clock,
    pool: &Pool,
    senders: &mut [Sender],
    scenario: &Scenario,
) -> Vec<Measured> {
    let calls: &[Call] = match scenario.load {
        Load::Control => &[Call::Control],
        Load::Streams(_) => &[Call::Send, Call::TrySend],
    };
    let waker = future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
    let mut nanos = vec![Vec::with_capacity(ROUNDS); calls.len()];
    let mut allocations = vec![0; calls.len()];
    let mut held = Vec::with_capacity(SENDS);
    for round in 0..WARMUP + ROUNDS {
        for (at, &call) in calls.iter().enumerate() {
            clock.sleep(PAUSE).await;
            let blocks: Vec<Block> =
                (0..SENDS).map(|_| filled(pool, scenario.bytes)).collect();
            let (span, counted) = ALLOCATOR.count(|| match call {
                Call::Control => poll_control(clock, &waker, blocks, &mut held),
                Call::Send => poll_sends(senders, blocks),
                Call::TrySend => try_sends(senders, blocks),
            });
            held.clear();
            if round >= WARMUP {
                nanos[at].push(per(span) / per(SENDS));
                allocations[at] += counted;
            }
        }
    }
    let sends = u64::try_from(ROUNDS * SENDS).expect("fits");
    let lines = calls.iter().zip(nanos).zip(allocations);
    lines
        .map(|((&call, mut nanos), allocations)| {
            nanos.sort_unstable_by(f64::total_cmp);
            let (name, polls) = match call {
                Call::Control | Call::Send => (scenario.name.to_owned(), sends),
                Call::TrySend => (format!("try {}", scenario.name), 0),
            };
            Measured {
                name,
                nanos,
                allocations,
                polls,
                sends,
            }
        })
        .collect()
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

/// Calls `try_send` with each block once, on `senders` in turn, and gives the
/// nanoseconds it took.
///
/// # Panics
///
/// When a `try_send` gives its block back or fails.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn try_sends(senders: &mut [Sender], blocks: Vec<Block>) -> u64 {
    let mut blocks = blocks.into_iter();
    let start = Instant::now();
    while blocks.len() > 0 {
        for (sender, block) in senders.iter_mut().zip(&mut blocks) {
            let given = sender.try_send(block).expect("the try_send goes");
            assert!(
                given.is_none(),
                "a try_send gave its block back: raise PAUSE"
            );
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

/// Opens a stream for each class of `load`.
async fn open(session: &Session, load: Load) -> Vec<Sender> {
    let classes = match load {
        Load::Control => &[][..],
        Load::Streams(classes) => classes,
    };
    let mut senders = Vec::with_capacity(classes.len());
    for &class in classes {
        senders.push(session.open_sender(class).await.expect("a stream"));
    }
    senders
}

/// Runs the rounds of a `WAITING` scenario: per round, `senders` send [`SENDS`]
/// blocks at once, split by the share of their classes.
///
/// # Panics
///
/// When a send fails, when the rounds do not show `premise`, or when the classes the
/// server got in `accepted` are not those of `senders`.
async fn compete(
    pool: &Pool,
    senders: &mut [Sender],
    scenario: Scenario,
    premise: Premise,
    accepted: &Accepted,
) -> Measured {
    let classes: Vec<Class> = senders.iter().map(Sender::class).collect();
    let share = SENDS / senders.len();
    let mut nanos = Vec::with_capacity(ROUNDS);
    let (mut allocations, mut polls, mut timed) = (0, 0, 0);
    let (mut complete, mut latest) = (0, 0);
    for round in 0..WARMUP + ROUNDS {
        let tally = Tally::new(classes.clone(), premise);
        let tally = RefCell::new(tally);
        let mut sends: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
        for (at, sender) in senders.iter_mut().enumerate() {
            let blocks = (0..share).map(|_| filled(pool, scenario.bytes)).collect();
            sends.push(Box::pin(send_each(sender, blocks, at, &tally)));
        }
        join(sends).await;
        let tally = tally.into_inner();
        let sends: u64 = tally.sent.iter().sum();
        assert!(
            tally.shown,
            "{} is not {premise:?} in round {round}",
            scenario.name
        );
        assert!(
            2 * sends >= u64::try_from(SENDS).expect("fits"),
            "{} times {sends} of {SENDS} sends in round {round}",
            scenario.name,
        );
        if round >= WARMUP {
            nanos.push(per(tally.timed.nanos) / per(sends));
            allocations += tally.timed.allocations;
            polls += tally.timed.polls;
            timed += sends;
            complete += tally.sends(Class::Complete);
            latest += tally.sends(Class::Latest);
        }
    }
    if let Premise::Competes = premise {
        assert!(
            complete >= 2 * latest,
            "{} times {complete} Complete and {latest} Latest sends: under 2 to 1",
            scenario.name,
        );
    }
    let mut sent = mem::take(&mut *accepted.lock().expect("not poisoned"));
    let mut asked = classes.clone();
    for list in [&mut sent, &mut asked] {
        list.sort_by_key(|&class| classes.iter().position(|&listed| listed == class));
    }
    assert_eq!(sent, asked, "the classes {} sends on", scenario.name);
    nanos.sort_unstable_by(f64::total_cmp);
    Measured {
        name: scenario.name.to_owned(),
        nanos,
        allocations,
        polls,
        sends: timed,
    }
}

/// Sends each of `blocks` on `sender` in order, and adds each poll of a send to
/// `tally` as the sends of sender `at`.
async fn send_each(
    sender: &mut Sender,
    blocks: Vec<Block>,
    at: usize,
    tally: &RefCell<Tally>,
) {
    for block in blocks {
        let mut send = pin!(sender.send(block));
        future::poll_fn(|cx| tally.borrow_mut().poll(at, send.as_mut(), cx))
            .await
            .expect("the send goes");
    }
}

/// The polls of the sends of one `WAITING` round. Under `Premise::Competes`, a send
/// is timed only when its last poll runs while a send of the other class waits.
struct Tally {
    /// The cost of the timed sends.
    timed: Cost,
    /// The timed sends of each sender.
    sent: Vec<u64>,
    /// The cost so far of the current send of each sender.
    current: Vec<Cost>,
    /// The class of each sender.
    classes: Vec<Class>,
    /// Whether the last poll of the current send of each sender waited.
    waiting: Vec<bool>,
    /// Whether each sender finished a send while a send of the other class waited.
    turned: Vec<bool>,
    premise: Premise,
    /// Whether the polls so far show `premise`.
    shown: bool,
}

/// The time, allocations and polls of one or more sends.
#[derive(Clone, Copy, Default)]
struct Cost {
    nanos: u64,
    allocations: u64,
    polls: u64,
}

impl Tally {
    fn new(classes: Vec<Class>, premise: Premise) -> Self {
        Self {
            timed: Cost::default(),
            sent: vec![0; classes.len()],
            current: vec![Cost::default(); classes.len()],
            waiting: vec![false; classes.len()],
            turned: vec![false; classes.len()],
            classes,
            premise,
            shown: false,
        }
    }

    /// Polls `send` of sender `at` once, and adds the send's cost when it ends and is
    /// timed.
    #[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
    fn poll<F: Future>(
        &mut self,
        at: usize,
        send: Pin<&mut F>,
        cx: &mut Context<'_>,
    ) -> Poll<F::Output> {
        let other = match self.classes[at] {
            Class::Latest => Class::Complete,
            _ => Class::Latest,
        };
        let competes = self.waits(other);
        let start = Instant::now();
        let (polled, counted) = ALLOCATOR.count(|| send.poll(cx));
        let current = &mut self.current[at];
        current.nanos += nanos(Instant::now().duration_since(start));
        current.allocations += counted;
        current.polls += 1;
        self.waiting[at] = polled.is_pending();
        if polled.is_ready() {
            let cost = mem::take(current);
            self.turned[at] |= competes;
            if competes || matches!(self.premise, Premise::Waits) {
                self.timed.nanos += cost.nanos;
                self.timed.allocations += cost.allocations;
                self.timed.polls += cost.polls;
                self.sent[at] += 1;
            }
        }
        self.shown |= match self.premise {
            Premise::Waits => self.waiting.iter().any(|&waits| waits),
            Premise::Competes => {
                self.turned(Class::Latest) && self.turned(Class::Complete)
            }
        };
        polled
    }

    /// Whether the current send of a sender of `class` waits.
    fn waits(&self, class: Class) -> bool {
        let mut senders = self.classes.iter().zip(&self.waiting);
        senders.any(|(&of, &waits)| of == class && waits)
    }

    /// The timed sends of the senders of `class`.
    fn sends(&self, class: Class) -> u64 {
        let senders = self.classes.iter().zip(&self.sent);
        senders
            .filter(|&(&of, _)| of == class)
            .map(|(_, sent)| sent)
            .sum()
    }

    /// Whether a sender of `class` finished a send while the other class waited.
    fn turned(&self, class: Class) -> bool {
        let mut senders = self.classes.iter().zip(&self.turned);
        senders.any(|(&of, &turned)| of == class && turned)
    }
}

/// Runs `futures` at once. Each is polled only when it was woken, so no poll of a
/// send is spurious.
async fn join(mut futures: Vec<Pin<Box<dyn Future<Output = ()> + '_>>>) {
    let parent = future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
    let flags: Vec<_> = futures
        .iter()
        .map(|_| {
            Arc::new(Flag {
                woken: AtomicBool::new(true),
                parent: parent.clone(),
            })
        })
        .collect();
    let wakers: Vec<_> = flags.iter().cloned().map(Waker::from).collect();
    let mut done = vec![false; futures.len()];
    future::poll_fn(|cx| {
        assert!(
            cx.waker().will_wake(&parent),
            "the task's waker is the same"
        );
        for (at, future) in futures.iter_mut().enumerate() {
            if !done[at] && flags[at].woken.swap(false, Ordering::Relaxed) {
                let mut cx = Context::from_waker(&wakers[at]);
                done[at] = future.as_mut().poll(&mut cx).is_ready();
            }
        }
        if done.iter().all(|&done| done) {
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
    println!("ns per timed send over {ROUNDS} rounds of {SENDS} sends");
    println!("pN: the round at percentile N, over its timed sends");
    println!(
        "{:<34} {:>9} {:>9} {:>9} {:>12} {:>11} {:>6}",
        "scenario", "p10", "p50", "p90", "allocs/send", "polls/send", "timed"
    );
    for line in lines {
        let at = |percent: usize| line.nanos[ROUNDS * percent / 100];
        let allocations = per(line.allocations) / per(line.sends);
        let polls = per(line.polls) / per(line.sends);
        let timed = per(line.sends) / per(ROUNDS * SENDS);
        let name = &line.name;
        let (p10, p50, p90) = (at(10), at(50), at(90));
        println!(
            "{name:<34} {p10:>9.1} {p50:>9.1} {p90:>9.1} {allocations:>12.2} \
             {polls:>11.2} {timed:>6.2}"
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
