//! The per-frame cost of the serve of a client write stream at the home of its index.
//! Run with `cargo bench -p hub --bench relay`.
//!
//! A program writes through the home's client session, one stream for each case. The
//! bench holds the serve future and polls it by hand. In each round the program sends
//! `FRAMES` frames, the bench waits until the transport holds them, and one timed poll
//! takes them, writes them, and sends their outcomes. A round that one poll does not
//! take whole is polled again outside the timing, and the title counts such rounds.
//! Each line gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each timed poll.
//! - `write 1x2`: the control, `Writer::write` alone of a frame of one sample of an
//!   index and a data channel.
//! - `write 2x(1x2)`, `write 8x(1x2)`: as `write 1x2`, of frames of two and eight
//!   groups, which one writer of the home takes whole.
//! - `live 1x2`: frames as the control, of `Time::Frame(Path::Live)`.
//! - `node 1x2`: as `live 1x2`, of `Time::Node`, so the node stamps the index.
//! - `live 100x2`: as `live 1x2`, with 100 samples per series.
//! - `live 1x64`: as `live 1x2`, with an index and 63 data channels.
//! - `waiting 1x2`: as `live 1x2`, while a writer of the home holds control of the
//!   channels, so the home refuses each frame as `Waiting`.
//! - `live 2x(1x2)`: frames of two groups, each as `live 1x2`, so the serve copies the
//!   series of each group into a draft of its own writer.
//! - `live 2x(1x64)`: as `live 2x(1x2)`, with an index and 63 data channels in
//!   each group.
//! - `live 8x(1x2)`: as `live 2x(1x2)`, with eight groups.
//! - `live 1/8x(1x2)`: as `live 8x(1x2)`, with frames that hold one group.
//!
//! Compare two builds on one pinned core whose SMT sibling is idle.

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
#[path = "../tests/common/net.rs"]
mod net;
#[path = "../tests/common/node.rs"]
mod node;
#[path = "../tests/common/serving.rs"]
mod serving;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/sessions.rs"]
mod sessions;
mod table;

use std::iter;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use agent::Agent;
use common::name;
use env::tasks::Tasks;
use fixture::{channel_name, create_hub};
use hub::home;
use hub::writer::{self, Writer};
use hub::{Hub, Served};
use table::Line;
use transport::Address;
use types::authority::Authority;
use types::frame::{Draft, Form, Label, Path};
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::client::Place;
use wire::hub::write::{Head, Outcome, Refusal, Reply, Time};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round.
const FRAMES: usize = 64;
const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// How long the bench waits between two checks of what the program did.
const STEP: Span = Span::MILLISECOND;
/// The keys of each case's channels start at a multiple of `LINE`.
const LINE: u128 = 1000;
/// The name and groups of each control line.
const CONTROLS: [(&str, u32); 3] =
    [("write 1x2", 1), ("write 2x(1x2)", 2), ("write 8x(1x2)", 8)];

/// A write stream that the bench serves.
struct Case {
    name: &'static str,
    /// The frames are `Time::Node`, else `Time::Frame(Path::Live)`.
    node: bool,
    /// Samples per series.
    count: u32,
    /// Groups of the write, each on an index of its own.
    groups: u32,
    /// Groups that each frame holds: those of the first places.
    present: u32,
    /// Series per group: the index and its data channels.
    series: u32,
    /// A writer of the home holds control, so each frame is refused as `Waiting`.
    waiting: bool,
}

const CASES: [Case; 9] = [
    Case {
        name: "live 1x2",
        node: false,
        count: 1,
        groups: 1,
        present: 1,
        series: 2,
        waiting: false,
    },
    Case {
        name: "node 1x2",
        node: true,
        count: 1,
        groups: 1,
        present: 1,
        series: 2,
        waiting: false,
    },
    Case {
        name: "live 100x2",
        node: false,
        count: 100,
        groups: 1,
        present: 1,
        series: 2,
        waiting: false,
    },
    Case {
        name: "live 1x64",
        node: false,
        count: 1,
        groups: 1,
        present: 1,
        series: 64,
        waiting: false,
    },
    Case {
        name: "waiting 1x2",
        node: false,
        count: 1,
        groups: 1,
        present: 1,
        series: 2,
        waiting: true,
    },
    Case {
        name: "live 2x(1x2)",
        node: false,
        count: 1,
        groups: 2,
        present: 2,
        series: 2,
        waiting: false,
    },
    Case {
        name: "live 2x(1x64)",
        node: false,
        count: 1,
        groups: 2,
        present: 2,
        series: 64,
        waiting: false,
    },
    Case {
        name: "live 8x(1x2)",
        node: false,
        count: 1,
        groups: 8,
        present: 8,
        series: 2,
        waiting: false,
    },
    Case {
        name: "live 1/8x(1x2)",
        node: false,
        count: 1,
        groups: 8,
        present: 1,
        series: 2,
        waiting: false,
    },
];

/// The index key of case `at`: its data keys follow it.
fn base(at: usize) -> u128 {
    u128::try_from(at + 1).expect("few") * LINE
}

/// The index key of group `group` of case `at`: its data keys follow it.
fn group(at: usize, group: u32) -> u128 {
    base(at) + u128::from(group) * LINE / 8
}

/// What the program and the home tell each other.
#[derive(Default)]
struct Progress {
    /// The first stamp of the program's frames, after the control line's.
    stamp: AtomicI64,
    /// The write streams that have their places.
    opened: AtomicUsize,
    /// The rounds that the home lets the program send.
    go: AtomicUsize,
    /// The frames that the program sent.
    sent: AtomicUsize,
    /// The frames whose outcome the program took.
    frames: AtomicUsize,
    /// The `Stored` replies that the program took.
    stored: AtomicUsize,
}

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let home = sim.node(sim::node::Config::default());
    let progress = Arc::new(Progress::default());
    let at = Address::Udp(std::net::SocketAddr::new(home.addresses()[0], net::PORT));
    let node = sim.node(sim::node::Config::default());
    let shared = Arc::clone(&progress);
    let own = node.clone();
    let config = env::shards::Config {
        name: "program".into(),
        core: None,
    };
    let main = move |tasks: Tasks| async move {
        while shared.stamp.load(Ordering::Relaxed) == 0 {
            own.clock().sleep(STEP).await;
        }
        program(&own, tasks, at, &shared).await;
    };
    drop(node.shards().start(config, main).expect("starts"));
    let (timer, lines, partial, stored) = sim
        .run_on(&home, move |node, tasks| bench(node, tasks, progress))
        .expect("the run ends");
    let title = format!(
        "ns per frame over {ROUNDS} rounds of {FRAMES} frames \
         (rounds not whole in one poll: {partial}; Stored replies: {stored})"
    );
    table::print(&title, &timer, &lines);
}

/// The home: runs the control line, then serves the program's write of each case.
async fn bench(
    node: sim::node::Node,
    tasks: Tasks,
    progress: Arc<Progress>,
) -> (Line, Vec<Line>, usize, usize) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
    let indexes = (0..=CASES.len())
        .flat_map(|at| {
            let groups = CASES.get(at).map_or(8, |case| case.groups);
            (0..groups).map(move |g| group(at, g))
        })
        .map(|index| (index, index + 1..=index + 63));
    let (hub, now) = create_hub(&node, tasks.clone(), indexes).await;
    hub.set_rules(sessions::rules());
    let mut timer = Line::new("timer", 1);
    let mut lines = Vec::new();
    let mut stamp = now - 10_000_000;
    for (name, groups) in CONTROLS {
        let (line, after) =
            control(&node, &hub, (name, groups), stamp, &mut timer).await;
        lines.push(line);
        stamp = after;
    }
    progress.stamp.store(stamp, Ordering::Relaxed);
    let (session, remote) = serving::program(&transport, &hub, &tasks).await;
    let mut partial = 0;
    for (at, case) in CASES.iter().enumerate() {
        let holder = if case.waiting {
            Some(holder(&hub, at).await)
        } else {
            None
        };
        let accepted = net::stream(&session).await;
        let mut serving = pin!(remote.serve(accepted));
        let mut line = Line::new(case.name, FRAMES);
        let opened = progress.opened.load(Ordering::Relaxed);
        while progress.opened.load(Ordering::Relaxed) == opened {
            serving::poll(serving.as_mut());
            node.clock().sleep(STEP).await;
        }
        node.clock().sleep(common::SETTLE).await;
        serving::poll(serving.as_mut());
        for round in 0..WARMUP + ROUNDS {
            let sent = progress.sent.load(Ordering::Relaxed) + FRAMES;
            let frames = progress.frames.load(Ordering::Relaxed) + FRAMES;
            progress.go.fetch_add(1, Ordering::Relaxed);
            while progress.sent.load(Ordering::Relaxed) < sent {
                node.clock().sleep(STEP).await;
            }
            node.clock().sleep(common::SETTLE).await;
            timer.add(table::timed(&ALLOCATOR, || ()).1);
            timer.close(round >= WARMUP);
            line.add(table::timed(&ALLOCATOR, || serving::poll(serving.as_mut())).1);
            let mut waits = 0;
            while progress.frames.load(Ordering::Relaxed) < frames {
                node.clock().sleep(STEP).await;
                waits += 1;
                if waits == 50 {
                    partial += 1;
                    serving::poll(serving.as_mut());
                    waits = 0;
                }
            }
            line.close(round >= WARMUP);
        }
        let served = serving.await;
        assert!(matches!(served, Ok(Served::Ended)), "{served:?}");
        drop(holder);
        lines.push(line);
    }
    drop(session.closed().await);
    let stored = progress.stored.load(Ordering::Relaxed);
    (timer, lines, partial, stored)
}

/// A writer that holds the first channel of case `at`, so that its program waits.
async fn holder(hub: &Hub, at: usize) -> Writer {
    let config = writer::Config {
        subject: name("holder"),
        authority: Authority(1),
        lease: None,
        channels: vec![channel_name(base(at) + 1)],
    };
    hub.writer(config).await.expect("opens")
}

/// The control line `name` of `groups`, whose first stamp is after `stamp`. Gives
/// the line and mesh time after its last frame.
async fn control(
    node: &sim::node::Node,
    hub: &Hub,
    (name, groups): (&'static str, u32),
    mut stamp: i64,
    timer: &mut Line,
) -> (Line, i64) {
    let mut line = Line::new(name, FRAMES);
    let channels = (0..groups).map(|g| channel_name(group(CASES.len(), g) + 1));
    let config = writer::Config {
        subject: common::name("control"),
        authority: Authority(1),
        lease: None,
        channels: channels.collect(),
    };
    let mut writer = hub.writer(config).await.expect("opens");
    let series: Vec<_> = (0..writer.set().entries().len())
        .map(|entry| (entry, 8))
        .collect();
    for round in 0..WARMUP + ROUNDS {
        for _ in 0..FRAMES {
            let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
            stamp += 1;
            for &(entry, _) in &series {
                let bytes = draft.series_mut(entry).expect("present");
                bytes.copy_from_slice(&stamp.to_le_bytes());
            }
            draft.set_counts(0, iter::repeat_n(1, series.len() / 2));
            timer.add(table::timed(&ALLOCATOR, || ()).1);
            timer.close(round >= WARMUP);
            line.add(table::timed(&ALLOCATOR, || write(&mut writer, draft)).1);
        }
        node.clock().sleep(common::SETTLE).await;
        line.close(round >= WARMUP);
    }
    (line, writer.now().nanos())
}

/// Writes `draft` with `writer`, whose home applies each group.
fn write(writer: &mut Writer, draft: Draft) {
    let outcomes = writer
        .write(Label::Path(Path::Live), draft)
        .expect("the home takes it");
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, home::Outcome::Applied { .. })),
        "the home applies the frame: {outcomes:?}"
    );
}

/// The program: opens a write for each case, and sends each round when the home lets
/// it.
async fn program(
    node: &sim::node::Node,
    tasks: Tasks,
    at: Address,
    progress: &Progress,
) {
    let mut agent = Agent::dial(node, tasks, at, name(agent::SUBJECT)).await;
    agent.admit().await;
    let mut stamp = progress.stamp.load(Ordering::Relaxed);
    let mut round = 0;
    for (at, case) in CASES.iter().enumerate() {
        let names: Vec<String> = (0..case.groups)
            .flat_map(|g| {
                (1..case.series).map(move |data| group(at, g) + u128::from(data))
            })
            .map(|key| channel_name(key).to_string())
            .collect();
        let select: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut stream = agent.write(&select).await;
        let start = stream.started().await;
        let series = usize::try_from(case.series * case.groups).expect("few");
        assert_eq!(start.places.len(), series, "the places of {}", case.name);
        progress.opened.fetch_add(1, Ordering::Relaxed);
        for _ in 0..WARMUP + ROUNDS {
            round += 1;
            while progress.go.load(Ordering::Relaxed) < round {
                node.clock().sleep(Span::from_nanos(100_000)).await;
            }
            for _ in 0..FRAMES {
                let (ends, body) = frame(case, &start.places, stamp);
                stamp += i64::from(case.count);
                stream.frame(head(case), &counts(case), &ends, &body).await;
            }
            progress.sent.fetch_add(FRAMES, Ordering::Relaxed);
            let mut outcomes = 0;
            while outcomes < FRAMES * usize::try_from(case.present).expect("few") {
                match stream.reply().await {
                    Reply::Outcome {
                        outcome: Outcome::Taken { .. },
                        ..
                    } => outcomes += 1,
                    Reply::Outcome {
                        outcome: Outcome::Refused(Refusal::Waiting),
                        ..
                    } if case.waiting => outcomes += 1,
                    Reply::Stored { .. } => {
                        progress.stored.fetch_add(1, Ordering::Relaxed);
                    }
                    other @ (Reply::Outcome { .. } | Reply::Ended { .. }) => {
                        panic!("not taken: {other:?}")
                    }
                }
            }
            progress.frames.fetch_add(FRAMES, Ordering::Relaxed);
        }
        stream.sender.finish().expect("finishes");
        while let Some(message) = stream.recv().await.expect("receives") {
            match Reply::decode(&message).expect("a reply") {
                Reply::Stored { .. } => {
                    progress.stored.fetch_add(1, Ordering::Relaxed);
                }
                other @ (Reply::Outcome { .. } | Reply::Ended { .. }) => {
                    panic!("not stored: {other:?}")
                }
            }
        }
    }
    agent.session.close(transport::Code(0));
    node.clock().sleep(STEP).await;
}

/// The head of each frame of `case`.
fn head(case: &Case) -> Head {
    Head {
        time: if case.node {
            Time::Node
        } else {
            Time::Frame(Path::Live)
        },
        form: Form::Raw,
        series: case.series * case.present,
    }
}

/// The count of each group of a frame of `case`.
fn counts(case: &Case) -> Vec<u32> {
    vec![case.count; usize::try_from(case.present).expect("few")]
}

/// The ends and body of a frame of `case` whose first stamp is `stamp`. The data
/// values are mixed, so that no codec finds a run.
fn frame(case: &Case, places: &[Place], stamp: i64) -> (Vec<(u32, u32)>, Vec<u8>) {
    let mut body = Vec::new();
    let mut ends = Vec::new();
    let present = usize::try_from(case.present).expect("few");
    let mut indexes = Vec::new();
    for info in places {
        if indexes.len() < present && !indexes.contains(&info.index) {
            indexes.push(info.index);
        }
    }
    for (place, info) in places.iter().enumerate() {
        if !indexes.contains(&info.index) {
            continue;
        }
        let index = info.data_type == Type::Scalar(Scalar::Stamp);
        for sample in 0..i64::from(case.count) {
            let value = if case.node && index {
                0
            } else if index {
                stamp + sample
            } else {
                let s = (stamp + sample).cast_unsigned();
                (s.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) ^ s)
                    .cast_signed()
            };
            body.extend_from_slice(&value.to_le_bytes());
        }
        let place = u32::try_from(place).expect("few");
        ends.push((place, u32::try_from(body.len()).expect("fits")));
    }
    (ends, body)
}
