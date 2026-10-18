//! The per-frame cost of `Link::serve` at the home of a remote reader session, over a
//! sim transport. Run with `cargo bench -p hub --bench serve`.
//!
//! The home serves the sessions of two peer nodes, whose transports take messages of at
//! most 1472 bytes and 64 KiB, and then the reads of a program. The bench holds the
//! future of `Link::serve` and polls it by hand, so a timed poll is the home's work for
//! the frames it sends. After each round of `FRAMES` frames, the bench waits until the
//! peer has read each of them, so no timed poll waits on the transport, and then past
//! the commit of the round. The writes of a round take no sim time, so with no wait
//! one commit takes the writes of many rounds, more than the ring holds. Each line
//! gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each timed poll.
//! - `narrow 1472`, `narrow 64k`: a latest session of 64 channels on one index, whose
//!   frames hold one sample of each. A write, then one poll, which sends that frame.
//! - `wide 1472`, `wide 64k`: as `narrow`, with an index and one data channel of
//!   `SAMPLES` samples per frame. At 1472 bytes the body takes two messages.
//! - `one set 1472`: the control of `two sets`, with one key set: the `narrow` session,
//!   while a writer of the index and half of its data channels writes, and closes and
//!   opens again before each frame.
//! - `complete 1472`: a complete session of the `narrow` channels. The round's writes
//!   and their commit, then one poll, which sends the round's frames. The open grants
//!   the bytes of the run, so no frame waits for credit.
//! - `complete half 1472`: as `complete`, with a session of the index and half the data
//!   channels, so the home walks each frame to charge it and to lay it.
//! - `two sets 1472`: the `narrow` session, while two writers of half of its data
//!   channels each write in turn, so each frame has another key set than the one
//!   before. Each writer closes before the other opens, outside the timed poll.
//! - `client`, `client complete`: a client read of the `narrow` channels by name, in
//!   latest and complete, timed as `narrow 1472` and `complete 1472`. A program dials
//!   with the default transport of `transport::Client`, once the HUB WIRE peers end.
//!
//! As for the `reader` bench, judge a change by `net`, and compare two builds on one
//! pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/agent.rs"]
mod agent;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/net.rs"]
mod net;
#[path = "../tests/common/node.rs"]
mod node;
#[path = "../tests/common/peer.rs"]
mod peer;
#[path = "../tests/common/serving.rs"]
mod serving;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/sessions.rs"]
mod sessions;
mod table;

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent::Agent;
use block::{Heap, Pool};
use common::name;
use env::tasks::Tasks;
use hub::writer::{self, Writer};
use hub::{Hub, Link, Served};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use table::Line;
use transport::stream::Receiver;
use transport::{Address, Transport};
use types::authority::Authority;
use types::channel;
use types::frame::key_set::Interner;
use types::frame::{Draft, Form};
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};
use wire::hub::{Mode, client};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round.
const FRAMES: usize = 64;
const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// The frames of each session.
const RUN: usize = (WARMUP + ROUNDS) * FRAMES;
/// Samples per series of a `wide` frame.
const SAMPLES: usize = 256;
/// A pool of 4 times the budget of `home::testing::shard`. A size class keeps its
/// range while one of its blocks is in use, so the lines before `two sets 1472` hold
/// most of 8 MiB. In some orders of the tasks, 8 MiB then has no room for a frame of
/// `two sets 1472` or of `wide 64k`.
const BUDGET: u64 = 1 << 25;
/// How long the bench waits between two checks of what the peer read.
const STEP: Span = Span::MILLISECOND;

/// The keys of each line's channels start at a multiple of `LINE`, so that a latest
/// session opens on an index with no frame to send.
const LINE: u128 = 1000;
/// From the first key of a line: the index of the `narrow` channels, and their data
/// channels.
const NARROW: u128 = 1;
const DATA: RangeInclusive<u128> = 2..=64;
/// From the first key of a line: the index of the `wide` channels, and its data
/// channel.
const WIDE: u128 = 101;
const WIDE_DATA: u128 = 102;

/// The channels and writers of a line.
#[derive(Clone, Copy)]
enum Shape {
    Narrow,
    Wide,
    /// The `narrow` session, and a writer of the index and half its data channels.
    OneSet,
    TwoSets,
    /// The `narrow` writer, and a session of the index and half its data channels.
    Half,
}

impl Shape {
    /// The samples of each series of a frame.
    fn samples(self) -> usize {
        match self {
            Self::Narrow | Self::OneSet | Self::TwoSets | Self::Half => 1,
            Self::Wide => SAMPLES,
        }
    }

    /// The key sets of the line's writers from `base`.
    fn sets(self, base: u128) -> Vec<Vec<u128>> {
        match self {
            Self::Narrow | Self::Half => vec![narrow(base).collect()],
            Self::Wide => vec![vec![base + WIDE, base + WIDE_DATA]],
            Self::OneSet => vec![first(base), first(base)],
            Self::TwoSets => vec![
                first(base),
                narrow(base)
                    .filter(|&key| key == base + NARROW || key > half(base))
                    .collect(),
            ],
        }
    }

    /// The keys that the session reads from `base`, in order.
    fn read(self, base: u128) -> Vec<u128> {
        match self {
            Self::Narrow | Self::OneSet | Self::TwoSets => narrow(base).collect(),
            Self::Wide => vec![base + WIDE, base + WIDE_DATA],
            Self::Half => first(base),
        }
    }
}

/// The keys of the `narrow` channels from `base`, in order.
fn narrow(base: u128) -> impl Iterator<Item = u128> {
    std::iter::once(NARROW)
        .chain(DATA)
        .map(move |key| base + key)
}

/// The index and the first half of the `narrow` data channels from `base`.
fn first(base: u128) -> Vec<u128> {
    narrow(base).filter(|&key| key <= half(base)).collect()
}

/// The last key of the first half of the `narrow` data channels from `base`.
fn half(base: u128) -> u128 {
    base + (DATA.end() - DATA.start()) / 2 + DATA.start()
}

/// The first key of the channels of line `at` of peer `turn`.
fn base(turn: usize, at: usize) -> u128 {
    let before: usize = PEERS[..turn].iter().map(|(_, cases)| cases.len()).sum();
    u128::try_from(before + at).expect("few lines") * LINE
}

/// One line: one session of a peer.
struct Case {
    name: &'static str,
    mode: Mode,
    shape: Shape,
    /// The messages of each frame: the head, the ends, and the body.
    messages: usize,
}

const LATEST: Mode = Mode::Latest;
const COMPLETE: Mode = Mode::Complete {
    limit_bytes: 1 << 40,
};

/// How a peer reaches the home.
#[derive(Clone, Copy)]
enum End {
    /// A HUB WIRE peer, whose transport takes messages of at most this many bytes.
    Wire(usize),
    /// A program.
    Client,
}

/// The lines of each peer, in the order in which they dial.
const PEERS: [(End, &[Case]); 3] = [
    (
        End::Wire(1472),
        &[
            Case {
                name: "narrow 1472",
                mode: LATEST,
                shape: Shape::Narrow,
                messages: 3,
            },
            Case {
                name: "wide 1472",
                mode: LATEST,
                shape: Shape::Wide,
                messages: 4,
            },
            Case {
                name: "complete 1472",
                mode: COMPLETE,
                shape: Shape::Narrow,
                messages: 3,
            },
            Case {
                name: "complete half 1472",
                mode: COMPLETE,
                shape: Shape::Half,
                messages: 3,
            },
            Case {
                name: "one set 1472",
                mode: LATEST,
                shape: Shape::OneSet,
                messages: 3,
            },
            Case {
                name: "two sets 1472",
                mode: LATEST,
                shape: Shape::TwoSets,
                messages: 3,
            },
        ],
    ),
    (
        End::Wire(1 << 16),
        &[
            Case {
                name: "narrow 64k",
                mode: LATEST,
                shape: Shape::Narrow,
                messages: 3,
            },
            Case {
                name: "wide 64k",
                mode: LATEST,
                shape: Shape::Wide,
                messages: 3,
            },
        ],
    ),
    (
        End::Client,
        &[
            Case {
                name: "client",
                mode: LATEST,
                shape: Shape::Narrow,
                messages: 3,
            },
            Case {
                name: "client complete",
                mode: COMPLETE,
                shape: Shape::Narrow,
                messages: 3,
            },
        ],
    ),
];

/// What the peers tell the home.
#[derive(Default)]
struct Progress {
    /// The peer whose turn it is to dial.
    turn: AtomicUsize,
    /// The sessions that the home opened.
    opened: AtomicUsize,
    /// The frames that the peers read in full.
    frames: AtomicUsize,
}

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let home = sim.node(sim::node::Config::default());
    let progress = Arc::new(Progress::default());
    let at = Address::Udp(std::net::SocketAddr::new(home.addresses()[0], net::PORT));
    for (turn, (end, _)) in PEERS.into_iter().enumerate() {
        let node = sim.node(sim::node::Config::default());
        let progress = Arc::clone(&progress);
        let config = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = node.clone();
        let main = move |tasks: Tasks| async move {
            while progress.turn.load(Ordering::Relaxed) != turn {
                own.clock().sleep(STEP).await;
            }
            match end {
                End::Wire(message) => {
                    peer(&own, &tasks, message, at, turn, &progress).await;
                }
                End::Client => program(&own, tasks, at, turn, &progress).await,
            }
            progress.turn.fetch_add(1, Ordering::Relaxed);
        };
        drop(node.shards().start(config, main).expect("starts"));
    }
    let (timer, lines) = sim
        .run_on(&home, move |node, tasks| bench(node, tasks, progress))
        .expect("the run ends");
    let title = format!("ns per frame over {ROUNDS} rounds of {FRAMES} frames");
    table::print(&title, &timer, &lines);
}

/// The home: serves each session of each peer in turn, and gives the `timer` line and
/// a line for each session.
async fn bench(
    node: sim::node::Node,
    tasks: Tasks,
    progress: Arc<Progress>,
) -> (Line, Vec<Line>) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
    let (hub, mut stamp) = create_hub(&node, tasks.clone()).await;
    hub.set_rules(sessions::rules());
    let mut timer = Line::new("timer", FRAMES);
    let mut lines = Vec::new();
    for (turn, (end, cases)) in PEERS.into_iter().enumerate() {
        let (session, remote) = accept(&transport, &hub, &tasks, end).await;
        for (at, case) in cases.iter().enumerate() {
            let mut serving = pin!(remote.serve(net::stream(&session).await));
            let mut line = Line::new(case.name, FRAMES);
            let configs = configs(base(turn, at), case.shape);
            let mut writer = hub.writer(configs[0].clone()).await.expect("opens");
            let opened = progress.opened.load(Ordering::Relaxed);
            while progress.opened.load(Ordering::Relaxed) == opened {
                serving::poll(serving.as_mut());
                node.clock().sleep(STEP).await;
            }
            for round in 0..WARMUP + ROUNDS {
                let frames = progress.frames.load(Ordering::Relaxed) + FRAMES;
                match case.mode {
                    Mode::Latest => {
                        for frame in 0..FRAMES {
                            if configs.len() > 1 {
                                drop(writer);
                                let config = configs[frame % configs.len()].clone();
                                writer = hub.writer(config).await.expect("opens");
                            }
                            let frame =
                                draft(&writer, case.shape.samples(), &mut stamp);
                            common::write(&mut writer, frame);
                            timer.add(table::timed(&ALLOCATOR, || ()).1);
                            line.add(
                                table::timed(&ALLOCATOR, || {
                                    serving::poll(serving.as_mut());
                                })
                                .1,
                            );
                        }
                    }
                    Mode::Complete { .. } => {
                        for _ in 0..FRAMES {
                            let frame =
                                draft(&writer, case.shape.samples(), &mut stamp);
                            common::write(&mut writer, frame);
                            timer.add(table::timed(&ALLOCATOR, || ()).1);
                        }
                        node.clock().sleep(common::SETTLE).await;
                        line.add(
                            table::timed(&ALLOCATOR, || {
                                serving::poll(serving.as_mut());
                            })
                            .1,
                        );
                    }
                }
                read(&node, &progress, frames).await;
                node.clock().sleep(common::SETTLE).await;
                timer.close(round >= WARMUP);
                line.close(round >= WARMUP);
            }
            let served = serving.await;
            assert!(matches!(served, Ok(Served::Ended)), "{served:?}");
            lines.push(line);
        }
        drop(session.closed().await);
    }
    (timer, lines)
}

/// Accepts the session of the peer of `end`, and gives it and its link.
async fn accept(
    transport: &Transport,
    hub: &Hub,
    tasks: &Tasks,
    end: End,
) -> (transport::Session, Link) {
    match end {
        End::Wire(_) => {
            let session = transport.accept().await.expect("a session");
            let link = hub.link(session.clone());
            (session, link)
        }
        End::Client => serving::program(transport, hub, tasks).await,
    }
}

/// Waits until the peers have read `frames` frames in full.
///
/// # Panics
///
/// When they read more.
async fn read(node: &sim::node::Node, progress: &Progress, frames: usize) {
    while progress.frames.load(Ordering::Relaxed) < frames {
        node.clock().sleep(STEP).await;
    }
    assert_eq!(
        progress.frames.load(Ordering::Relaxed),
        frames,
        "the peer reads each frame of the round once"
    );
}

/// The HUB WIRE peer of `turn`: dials the home at `at`, and reads each of its
/// sessions in turn as [`take`] does.
async fn peer(
    node: &sim::node::Node,
    tasks: &Tasks,
    message: usize,
    at: Address,
    turn: usize,
    progress: &Progress,
) {
    let pool = net::own_pool();
    let transport: Transport = net::transport(node, tasks, &pool, net::PEER, message);
    let session = transport
        .dial(net::public_key(&net::HOME), &[at])
        .await
        .expect("dials");
    for (line, case) in PEERS[turn].1.iter().enumerate() {
        let keys = case.shape.read(base(turn, line));
        let (mut sender, mut receiver, reader) =
            peer::open(&pool, &session, case.mode, &keys).await;
        progress.opened.fetch_add(1, Ordering::Relaxed);
        take(&mut receiver, reader, case, progress).await;
        sender.finish().expect("finishes");
        let finished = receiver.recv().await;
        assert!(
            matches!(finished, Ok(None)),
            "the home finishes: {finished:?}"
        );
    }
    session.close(transport::Code(0));
    node.clock().sleep(STEP).await;
}

/// The program of `turn`: dials the home at `at`, admits a hello, and reads each of
/// its reads in turn as [`take`] does.
async fn program(
    node: &sim::node::Node,
    tasks: Tasks,
    at: Address,
    turn: usize,
    progress: &Progress,
) {
    let mut agent = Agent::dial(node, tasks, at, name(agent::SUBJECT)).await;
    agent.admit().await;
    for (line, case) in PEERS[turn].1.iter().enumerate() {
        let mode = match case.mode {
            Mode::Latest => client::Mode::Latest,
            Mode::Complete { .. } => client::Mode::Complete,
        };
        let names: Vec<_> = case
            .shape
            .read(base(turn, line))
            .into_iter()
            .map(|key| channel_name(key).to_string())
            .collect();
        let select: Vec<_> = names.iter().map(String::as_str).collect();
        let mut stream = agent.read(mode, &select).await;
        let (_, _, reader) = stream.opened().await;
        progress.opened.fetch_add(1, Ordering::Relaxed);
        take(&mut stream.receiver, reader, case, progress).await;
        stream.sender.finish().expect("finishes");
        let finished = stream.recv().await;
        assert!(
            matches!(finished, Ok(None)),
            "the home finishes: {finished:?}"
        );
    }
    agent.session.close(transport::Code(0));
    node.clock().sleep(STEP).await;
}

/// Reads `RUN` frames of `case` from `receiver` with `reader`, and counts each in
/// `progress`.
///
/// # Panics
///
/// When a frame does not take the messages of `case`.
async fn take(
    receiver: &mut Receiver,
    mut reader: wire::hub::Reader,
    case: &Case,
    progress: &Progress,
) {
    for _ in 0..RUN {
        let messages = peer::frame(receiver, &mut reader).await;
        assert_eq!(
            messages, case.messages,
            "the messages of a frame of {}",
            case.name
        );
        progress.frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// A hub on the shard of [`create_shard`], with the channels of each shape defined,
/// and the node's mesh time now.
async fn create_hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
    let (home, interner, now, time) = create_shard(node, tasks.clone()).await;
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: net::NODE,
        time,
        entropy: node.entropy(),
        region: None,
    });
    let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
    let mut definitions = Vec::new();
    let groups = PEERS.iter().enumerate().flat_map(|(turn, (_, cases))| {
        (0..cases.len()).flat_map(move |at| {
            let base = base(turn, at);
            [
                (base + NARROW, base + DATA.start()..=base + DATA.end()),
                (base + WIDE, base + WIDE_DATA..=base + WIDE_DATA),
            ]
        })
    });
    for (index, data) in groups {
        let key = channel::Key::from_u128(index);
        let kind = Kind::Index {
            error: None,
            control: None,
        };
        definitions.push((index, Channel { key, kind }));
        for data in data {
            let data_key = channel::Key::from_u128(data);
            let kind =
                Kind::Data(Data::new(key, None, i64.clone(), None).expect("no unit"));
            definitions.push((
                data,
                Channel {
                    key: data_key,
                    kind,
                },
            ));
        }
    }
    let definitions: Vec<_> = definitions
        .into_iter()
        .map(|(key, channel)| (channel_name(key), Definition::Channel(channel)))
        .collect();
    hub.set_definitions(
        definitions
            .iter()
            .map(|(name, definition)| (name, definition)),
    );
    (hub, now)
}

fn channel_name(key: u128) -> types::name::Name {
    name(&format!("c{key}"))
}

/// The writers of `shape` from `base`. Two writers on one index cannot both hold its
/// control, so a line of more than one closes each writer before it opens the next.
fn configs(base: u128, shape: Shape) -> Vec<writer::Config> {
    shape
        .sets(base)
        .into_iter()
        .enumerate()
        .map(|(at, keys)| writer::Config {
            subject: name(&format!("bench{at}")),
            authority: Authority(1),
            lease: None,
            channels: keys.into_iter().map(channel_name).collect(),
        })
        .collect()
}

/// A frame of `writer` whose series each hold `samples` samples, the next stamps from
/// `stamp`.
fn draft(writer: &Writer, samples: usize, stamp: &mut i64) -> Draft {
    let set = writer.set();
    let entries = set.entries();
    let series: Vec<_> = (0..entries.len())
        .map(|entry| (entry, samples * 8))
        .collect();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    let first = *stamp;
    *stamp += i64::try_from(samples).expect("few");
    for (at, entry) in entries.iter().enumerate() {
        let index = entry.data_type == Type::Scalar(Scalar::Stamp);
        let bytes = draft.series_mut(at).expect("the series is present");
        for (sample, stamp) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(first..) {
            let stamp = stamp.cast_unsigned();
            // Values that the codec cannot shrink, as a linear run shrinks to 24 bytes.
            let value = if index {
                stamp
            } else {
                stamp.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) ^ stamp
            };
            sample.copy_from_slice(&value.to_le_bytes());
        }
    }
    draft.set_count(entries[0].group, u32::try_from(samples).expect("few"));
    draft
}

/// `home::testing::shard` with a pool of `BUDGET`: a shard on a new ring of 4 MiB,
/// its interner, the mesh time once the clock has one, and the reader of that time.
async fn create_shard(
    node: &sim::node::Node,
    tasks: Tasks,
) -> (home::Shard, Interner, i64, clock::Reader) {
    let config = block::Config::new(BUDGET).expect("the budget fits");
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool,
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = home::Shard::open(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: home::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    loop {
        if let Some(now) = mesh.now().mesh {
            return (
                shard,
                interner,
                now.earliest.nanos().midpoint(now.latest.nanos()),
                mesh,
            );
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    }
}
