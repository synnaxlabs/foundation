//! The per-frame cost of each end of a forward of a program's write. Run with
//! `cargo bench -p hub --bench forward`.
//!
//! Each half runs one real end, a hub whose serve of the stream the bench holds and
//! polls by hand each time its waker is woken, and the other end by hand. In each
//! round the sender sends the frames of the round, the bench waits until the
//! transport holds them, and the timed polls run until the sender has the outcome of
//! each frame. Each line gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each timed poll.
//! - `member 1x2`: the member's serve of a program's write of frames of one sample of
//!   an index and a data channel, which it sends on to a home by hand, and the
//!   outcome of each back. 64 frames each round.
//! - `member 1x64`: as `member 1x2`, with an index and 63 data channels.
//! - `member 1x2 one`: as `member 1x2`, one frame each round.
//! - `member 1x2 full`: as `member 1x2`, 128 frames each round to a home that answers
//!   [`WINDOW`] at a time, so the member reads no more of the program while the
//!   window is full.
//! - `member 1x2 one+62`: as `member 1x2 one`, while 62 more forwards of the program's
//!   session wait for a renewal of its hello, so the timed forward keeps its waker in
//!   a set of 63.
//! - `direct 1x2`, `direct 1x64`, `direct 1x2 one`: the serve of a program's write of
//!   the frames of the member line of the same name at the home of its index, with no
//!   forward.
//! - `home 1x2`, `home 1x64`, `home 1x2 one`: the home's serve of a forward of the
//!   frames of the member line of the same name, which a member by hand sends.
//!
//! A forward adds the member line to a frame, and its home line less its direct line.
//! Each line runs on the `sim` driver and leaves out the carrier task of each
//! transport, which seals and decodes the packets (#3584). Compare two builds on one
//! pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/agent.rs"]
mod agent;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
mod fixture;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/forwarded.rs"]
mod forwarded;
#[path = "../tests/common/net.rs"]
mod net;
#[path = "../tests/common/node.rs"]
mod node;
#[path = "../tests/common/region.rs"]
mod region;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/serving.rs"]
mod serving;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/sessions.rs"]
mod sessions;
mod table;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use access::proof::Kind;
use agent::{AGENT, Agent, CONNECTION, LIFE, SUBJECT, Stream, name, place};
use env::tasks::Tasks;
use forwarded::{Forwarded, answer, run, send_places};
use hub::{Served, serve};
use spec::definition::Definition;
use table::Line;
use transport::{Address, Code};
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Path};
use types::hello::Hello;
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::client::Place;
use wire::hub::client::write::Open;
use wire::hub::write::{Head, Outcome, Reply, Time};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// How long the bench waits between two checks of what the other end did.
const STEP: Span = Span::MILLISECOND;
/// How long a round may take before the bench calls it stuck.
const ROUND_MAX: Span = Span::from_nanos(1_000_000_000);
/// The bytes of a message that the transport of each node takes.
const MESSAGE: usize = 1 << 16;
/// The streams that a peer may open at once in each session, as a node allows by
/// default.
const STREAMS: u32 = 64;
/// The index of each write.
const TIME: channel::Key = channel::Key::from_u128(1);
/// The most frames whose outcome the home by hand holds back, the window of a
/// forward.
const WINDOW: u64 = 64;

/// A write stream that one end serves.
struct Case {
    name: &'static str,
    /// Series per frame: the index and its data channels.
    series: u32,
    /// Frames per round.
    frames: usize,
    /// Forwards of the same session that wait while the case runs.
    idle: usize,
}

const MEMBER: [Case; 5] = [
    Case {
        name: "member 1x2",
        series: 2,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "member 1x64",
        series: 64,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "member 1x2 full",
        series: 2,
        frames: 128,
        idle: 0,
    },
    Case {
        name: "member 1x2 one",
        series: 2,
        frames: 1,
        idle: 0,
    },
    // The program's hello stream and the timed write take two of `STREAMS`.
    Case {
        name: "member 1x2 one+62",
        series: 2,
        frames: 1,
        idle: 62,
    },
];

const DIRECT: [Case; 3] = [
    Case {
        name: "direct 1x2",
        series: 2,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "direct 1x64",
        series: 64,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "direct 1x2 one",
        series: 2,
        frames: 1,
        idle: 0,
    },
];

const HOME: [Case; 3] = [
    Case {
        name: "home 1x2",
        series: 2,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "home 1x64",
        series: 64,
        frames: 64,
        idle: 0,
    },
    Case {
        name: "home 1x2 one",
        series: 2,
        frames: 1,
        idle: 0,
    },
];

/// What the sender and the bench tell each other.
#[derive(Default)]
struct Progress {
    /// The first stamp of the sender's frames.
    stamp: AtomicI64,
    /// The write streams that have their places.
    opened: AtomicUsize,
    /// The rounds that the bench lets the sender send.
    go: AtomicUsize,
    /// The frames that the sender sent.
    sent: AtomicUsize,
    /// The frames whose outcome the sender took.
    taken: AtomicUsize,
}

fn main() {
    let timer = Line::new("timer", 1);
    let (timer, mut lines) = member(timer);
    let (timer, direct) = direct(timer);
    lines.extend(direct);
    let (timer, home) = home(timer);
    lines.extend(home);
    let title = format!("ns per frame over {ROUNDS} rounds");
    table::print(&title, &timer, &lines);
}

/// The address of the hub transport of `node`.
fn address(node: &sim::node::Node) -> Address {
    Address::Udp(SocketAddr::new(node.addresses()[0], net::PORT))
}

/// The shard config of a node's main task.
fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: None,
    }
}

/// The member half: a program writes each case of [`MEMBER`] at a member, which
/// forwards it to a home by hand. Gives `timer` with the floor of each timed poll
/// added, and a line for each case.
fn member(timer: Line) -> (Line, Vec<Line>) {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [member, home, program] =
        [(); 3].map(|()| sim.node(sim::node::Config::default()));
    let (at, other) = (address(&member), address(&home));
    let node = home.clone();
    let main = move |tasks| answer_member(node, tasks);
    drop(home.shards().start(shard("home"), main).expect("starts"));
    let (node, kept) = (program.clone(), Arc::clone(&progress));
    let main = move |tasks| write(node, tasks, at, &MEMBER, kept);
    drop(
        program
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run_on(&member, move |node, tasks| {
        let others = vec![other];
        serve_program(node, tasks, others, region::OTHER, &MEMBER, progress, timer)
    })
    .expect("the run ends")
}

/// The direct half: a program writes each case of [`DIRECT`] at the home of its
/// index. Gives as [`member`] does.
fn direct(timer: Line) -> (Line, Vec<Line>) {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [home, program] = [(); 2].map(|()| sim.node(sim::node::Config::default()));
    let at = address(&home);
    let (node, kept) = (program.clone(), Arc::clone(&progress));
    let main = move |tasks| write(node, tasks, at, &DIRECT, kept);
    drop(
        program
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run_on(&home, move |node, tasks| {
        serve_program(node, tasks, Vec::new(), net::NODE, &DIRECT, progress, timer)
    })
    .expect("the run ends")
}

/// The home half: a member by hand forwards each case of [`HOME`] to a home. Gives
/// as [`member`] does.
fn home(timer: Line) -> (Line, Vec<Line>) {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [home, member] = [(); 2].map(|()| sim.node(sim::node::Config::default()));
    let at = address(&home);
    let (node, kept) = (member.clone(), Arc::clone(&progress));
    let main = move |tasks| forward(node, tasks, at, kept);
    drop(
        member
            .shards()
            .start(shard("member"), main)
            .expect("starts"),
    );
    sim.run_on(&home, move |node, tasks| {
        serve_home(node, tasks, progress, timer)
    })
    .expect("the run ends")
}

/// The node [`net::NODE`] of a region of the nodes at `others`, which names `home`
/// the home of [`TIME`]: serves the program's write of each of `cases`.
async fn serve_program(
    node: sim::node::Node,
    tasks: Tasks,
    others: Vec<Address>,
    home: types::node::Key,
    cases: &[Case],
    progress: Arc<Progress>,
    mut timer: Line,
) -> (Line, Vec<Line>) {
    let pool = net::own_pool();
    let sizes = (MESSAGE, 1 << 20);
    let transport =
        net::transport_sized(&node, &tasks, &pool, net::HOME, sizes, STREAMS);
    let transport = Rc::new(transport);
    let region =
        region::open(&node, &tasks, Rc::clone(&transport), others, definitions()).await;
    let mesh = region.mesh.clone();
    let (hub, now) = fixture::create_hub_in(&node, tasks.clone(), region).await;
    hub.set_rules(sessions::rules());
    mesh.set_home(TIME, home).await.expect("sets the home");
    progress.stamp.store(now - 10_000_000, Ordering::Relaxed);
    let (session, link) = serving::program(&transport, &hub, &tasks).await;
    let mut lines = Vec::new();
    for case in cases {
        for _ in 0..case.idle {
            let incoming = net::stream(&session).await;
            let served = link.serve(incoming);
            tasks.spawn(async move { drop(served.await) });
        }
        let incoming = net::stream(&session).await;
        let mut serving = Serving::new(link.serve(incoming));
        lines.push(serving.run(&node, case, &progress, &mut timer).await);
    }
    drop(session.closed().await);
    (timer, lines)
}

/// The home at [`net::NODE`] of [`TIME`], whose region names the member by hand
/// [`region::OTHER`]: serves its forward of each case.
async fn serve_home(
    node: sim::node::Node,
    tasks: Tasks,
    progress: Arc<Progress>,
    mut timer: Line,
) -> (Line, Vec<Line>) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, MESSAGE);
    let transport = Rc::new(transport);
    let region = region::open(
        &node,
        &tasks,
        Rc::clone(&transport),
        Vec::new(),
        definitions(),
    )
    .await;
    let mesh = region.mesh.clone();
    let (hub, now) = fixture::create_hub_in(&node, tasks.clone(), region).await;
    hub.set_rules(sessions::rules());
    mesh.set_home(TIME, net::NODE).await.expect("sets the home");
    progress.stamp.store(now - 10_000_000, Ordering::Relaxed);
    let session = transport.accept().await.expect("a session");
    let link = hub.link(session.clone());
    let mut lines = Vec::new();
    for case in &HOME {
        let incoming = net::stream(&session).await;
        let mut serving = Serving::new(link.serve(incoming));
        lines.push(serving.run(&node, case, &progress, &mut timer).await);
    }
    drop(session.closed().await);
    (timer, lines)
}

/// `time`, `value`, and 62 more data channels on `time`, so an open names at most 63.
fn definitions() -> BTreeMap<Name, Definition> {
    common::definitions(&more())
}

/// The data channels after `value`.
fn more() -> Vec<Name> {
    (0..62).map(|n| name(&format!("value-{n}"))).collect()
}

/// The data channels of a write of `case`.
fn channels(case: &Case) -> Vec<String> {
    let more = more().into_iter().map(|name| name.to_string());
    let count = usize::try_from(case.series - 1).expect("few");
    std::iter::once("value".to_owned())
        .chain(more)
        .take(count)
        .collect()
}

/// Sets its flag when woken.
#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The serve of one stream, which the bench polls each time its waker is woken.
struct Serving<F> {
    future: Pin<Box<F>>,
    woken: Arc<Flag>,
    waker: Waker,
}

impl<F: Future<Output = Result<Served, serve::Error>>> Serving<F> {
    fn new(serving: F) -> Self {
        let woken = Arc::new(Flag(AtomicBool::new(true)));
        Self {
            future: Box::pin(serving),
            waker: Waker::from(Arc::clone(&woken)),
            woken,
        }
    }

    /// Whether the serve was woken since the last poll, which this clears.
    fn woken(&self) -> bool {
        self.woken.0.swap(false, Ordering::Relaxed)
    }

    fn poll(&mut self) -> Poll<Result<Served, serve::Error>> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    /// Polls when woken until `done`.
    async fn until(&mut self, node: &sim::node::Node, done: impl Fn() -> bool) {
        let polled = || {
            if done() {
                return true;
            }
            if self.woken() {
                assert!(self.poll().is_pending(), "the serve ended");
            }
            false
        };
        wait(node, polled, || "the serve is stuck".into()).await;
    }

    /// Serves the write of `case` until its sender finishes it, and gives its line.
    async fn run(
        &mut self,
        node: &sim::node::Node,
        case: &Case,
        progress: &Progress,
        timer: &mut Line,
    ) -> Line {
        let opened = progress.opened.load(Ordering::Relaxed);
        self.until(node, || progress.opened.load(Ordering::Relaxed) > opened)
            .await;
        let mut line = Line::new(case.name, case.frames);
        for round in 0..WARMUP + ROUNDS {
            let warm = round >= WARMUP;
            let sent = progress.sent.load(Ordering::Relaxed) + case.frames;
            let due = progress.taken.load(Ordering::Relaxed) + case.frames;
            progress.go.fetch_add(1, Ordering::Relaxed);
            let sending = || progress.sent.load(Ordering::Relaxed) >= sent;
            wait(node, sending, || format!("round {round} is not sent")).await;
            node.clock().sleep(common::SETTLE).await;
            let end = node.clock().now() + ROUND_MAX;
            while progress.taken.load(Ordering::Relaxed) < due {
                assert!(node.clock().now() < end, "round {round} is stuck");
                if self.woken() {
                    timer.add(table::timed(&ALLOCATOR, || ()).1);
                    timer.close(warm);
                    let (poll, cost) = table::timed(&ALLOCATOR, || self.poll());
                    assert!(poll.is_pending(), "the serve ended: {poll:?}");
                    line.add(cost);
                }
                node.clock().sleep(STEP).await;
            }
            line.close(warm);
        }
        let end = node.clock().now() + ROUND_MAX;
        loop {
            assert!(node.clock().now() < end, "the serve did not end");
            if self.woken()
                && let Poll::Ready(served) = self.poll()
            {
                assert!(matches!(served, Ok(Served::Ended)), "{served:?}");
                return line;
            }
            node.clock().sleep(STEP).await;
        }
    }
}

/// The program: writes each of `cases` at the node at `at`, and sends each round when
/// the bench lets it.
async fn write(
    node: sim::node::Node,
    tasks: Tasks,
    at: Address,
    cases: &'static [Case],
    progress: Arc<Progress>,
) {
    let mut agent = Agent::dial(&node, tasks, at, name(SUBJECT)).await;
    agent.admit().await;
    let mut sender = Sender::new(&progress);
    for case in cases {
        let names = channels(case);
        let select: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut idle = Vec::new();
        for _ in 0..case.idle {
            let mut stream = agent.write(&select).await;
            stream.started().await;
            idle.push(stream);
        }
        let stream = agent.write(&select).await;
        sender.run(&node, case, stream, &progress).await;
        for stream in idle {
            finish(stream).await;
        }
    }
    agent.session.close(Code(0));
    node.clock().sleep(STEP).await;
}

/// Waits until `done`, and panics with the text of `stuck` after [`ROUND_MAX`].
async fn wait(
    node: &sim::node::Node,
    mut done: impl FnMut() -> bool,
    stuck: impl Fn() -> String,
) {
    let end = node.clock().now() + ROUND_MAX;
    while !done() {
        assert!(node.clock().now() < end, "{}", stuck());
        node.clock().sleep(STEP).await;
    }
}

/// The member by hand at [`region::OTHER`]: forwards a write of each case of [`HOME`]
/// to the home at `at`, and sends each round when the bench lets it.
async fn forward(
    node: sim::node::Node,
    tasks: Tasks,
    at: Address,
    progress: Arc<Progress>,
) {
    let (clock, mesh) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let ready =
        || progress.stamp.load(Ordering::Relaxed) != 0 && mesh.now().mesh.is_some();
    wait(&node, ready, || {
        "the home and the mesh time are not ready".into()
    })
    .await;
    let pool = net::own_pool();
    let transport = net::transport(&node, &tasks, &pool, net::PEER, MESSAGE);
    let public_key = net::public_key(&net::HOME);
    let session = transport.dial(public_key, &[at]).await.expect("dials");
    let mut sender = Sender::new(&progress);
    for case in &HOME {
        let hello = Hello {
            subject: name(SUBJECT),
            key: net::public_key(&AGENT),
            via: net::public_key(&net::PEER),
            connection: CONNECTION,
            nonce: [0; 16],
            expires: mesh.now().mesh.expect("mesh time").latest + LIFE,
        };
        let names = channels(case);
        let at: Vec<u32> = (0..u32::try_from(names.len()).expect("few")).collect();
        let places = u32::try_from(at.len()).expect("few");
        let mut stream = forwarded::forward(&session, &pool, places, hello).await;
        let open = Open {
            authority: Authority(1),
            lease: None,
            channels: names.iter().map(|channel| name(channel)).collect(),
        };
        stream.write(&open, Kind::Write, &AGENT).await;
        stream.send(&run(&at)).await;
        sender.run(&node, case, stream, &progress).await;
    }
    session.close(Code(0));
    node.clock().sleep(STEP).await;
}

/// The end that sends the frames of each case, with the stamp of its next frame.
struct Sender {
    stamp: i64,
    round: usize,
}

impl Sender {
    fn new(progress: &Progress) -> Self {
        Self {
            stamp: progress.stamp.load(Ordering::Relaxed),
            round: 0,
        }
    }

    /// Takes the places of `stream`, a write of `case`, sends each round when the bench
    /// lets it, and takes each outcome. Then finishes `stream`.
    async fn run(
        &mut self,
        node: &sim::node::Node,
        case: &Case,
        mut stream: Stream,
        progress: &Progress,
    ) {
        let start = stream.started().await;
        let series = usize::try_from(case.series).expect("few");
        assert_eq!(start.places.len(), series, "the places of {}", case.name);
        progress.opened.fetch_add(1, Ordering::Relaxed);
        let head = Head {
            time: Time::Frame(Path::Live),
            form: Form::Raw,
            series: case.series,
        };
        for _ in 0..WARMUP + ROUNDS {
            self.round += 1;
            let go = || progress.go.load(Ordering::Relaxed) >= self.round;
            wait(node, go, || format!("round {} does not go", self.round)).await;
            for _ in 0..case.frames {
                let (ends, body) = frame(&start.places, self.stamp);
                self.stamp += 1;
                stream.frame(head, &[1], &ends, &body).await;
            }
            progress.sent.fetch_add(case.frames, Ordering::Relaxed);
            let mut outcomes = 0;
            while outcomes < case.frames {
                match stream.reply().await {
                    Reply::Outcome {
                        outcome: Outcome::Taken { .. },
                        ..
                    } => outcomes += 1,
                    Reply::Stored { .. } => {}
                    other @ (Reply::Outcome { .. } | Reply::Ended { .. }) => {
                        panic!("not taken: {other:?}")
                    }
                }
            }
            progress.taken.fetch_add(case.frames, Ordering::Relaxed);
        }
        finish(stream).await;
    }
}

/// Finishes `stream`, and takes each reply until the other end finishes.
async fn finish(mut stream: Stream) {
    stream.sender.finish().expect("finishes");
    while let Some(message) = stream.recv().await.expect("receives") {
        match Reply::decode(&message).expect("a reply") {
            Reply::Stored { .. } => {}
            other @ (Reply::Outcome { .. } | Reply::Ended { .. }) => {
                panic!("not stored: {other:?}")
            }
        }
    }
}

/// The ends and body of a frame of one sample at `places`, whose stamp is `stamp`. The
/// data values are mixed, so that no codec finds a run.
fn frame(places: &[Place], stamp: i64) -> (Vec<(u32, u32)>, Vec<u8>) {
    let mut body = Vec::new();
    let mut ends = Vec::new();
    for (at, place) in places.iter().enumerate() {
        let value = if place.data_type == Type::Scalar(Scalar::Stamp) {
            stamp
        } else {
            let s = stamp.cast_unsigned() + u64::try_from(at).expect("few");
            (s.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) ^ s).cast_signed()
        };
        body.extend_from_slice(&value.to_le_bytes());
        let at = u32::try_from(at).expect("few");
        ends.push((at, u32::try_from(body.len()).expect("fits")));
    }
    (ends, body)
}

/// The home by hand at [`region::OTHER`]: takes each forward of the member, sends the
/// places of its write, answers each round of its timed forward, [`WINDOW`] frames at
/// a time, and finishes each forward once the member does.
async fn answer_member(node: sim::node::Node, tasks: Tasks) {
    let (pool, sizes) = (net::own_pool(), (MESSAGE, 1 << 20));
    let transport =
        net::transport_sized(&node, &tasks, &pool, net::PEER, sizes, STREAMS);
    let session = transport.accept().await.expect("a session");
    for case in &MEMBER {
        let mut idle = Vec::with_capacity(case.idle);
        for _ in 0..case.idle {
            idle.push(opened(&session).await);
        }
        let mut stream = opened(&session).await;
        let frames = u64::try_from(case.frames).expect("few");
        for round in 0..u64::try_from(WARMUP + ROUNDS).expect("few") {
            let (mut first, end) = (round * frames, (round + 1) * frames);
            while first < end {
                let last = end.min(first + WINDOW);
                answer(&mut stream, first..last).await;
                first = last;
            }
        }
        for stream in std::iter::once(stream).chain(idle) {
            ended(stream).await;
        }
    }
    drop(session.closed().await);
}

/// The next forward of `session`, with the places of its write sent.
async fn opened(session: &transport::Session) -> Stream {
    let Forwarded {
        mut stream, open, ..
    } = Forwarded::accept(session).await;
    let index = place(1, Type::Scalar(Scalar::Stamp), "time", 0);
    let data = open.channels.iter().zip(2..).map(|(channel, key)| Place {
        key: channel::Key::from_u128(key),
        data_type: Type::Scalar(Scalar::I64),
        name: channel.clone(),
        index: 0,
    });
    let places: Vec<Place> = std::iter::once(index).chain(data).collect();
    send_places(&mut stream, &places, MESSAGE).await;
    stream
}

/// Waits for the finish of `stream`, then finishes it.
async fn ended(mut stream: Stream) {
    while stream.recv().await.expect("receives").is_some() {}
    stream.sender.finish().expect("finishes");
}
