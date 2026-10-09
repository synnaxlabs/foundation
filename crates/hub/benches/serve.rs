//! The per-frame cost of `Link::serve` at the home of a remote reader session, over a
//! sim transport. Run with `cargo bench -p hub --bench serve`.
//!
//! The home serves the sessions of two peer nodes, whose transports take messages of at
//! most 1472 bytes and 64 KiB. The bench holds the future of `Link::serve` and polls it
//! by hand, so a timed poll is the home's work for the frames it sends. After each round
//! of `FRAMES` frames, the bench waits until the peer has read each of them, so no timed
//! poll waits on the transport. Each line gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `narrow 1472`, `narrow 64k`: a latest session of 64 channels on one index, whose
//!   frames hold one sample of each. A write, then one poll, which sends that frame.
//! - `wide 1472`, `wide 64k`: as `narrow`, with an index and one data channel of
//!   `SAMPLES` samples per frame. At 1472 bytes the body takes two messages.
//! - `complete 1472`: a complete session of the `narrow` channels. The round's writes
//!   and their commit, then one poll, which sends the round's frames. The open grants
//!   the bytes of the run, so no frame waits for credit.
//! - `two sets 1472`: the `narrow` session, while two writers of half of its data
//!   channels each write in turn, so each frame has another key set than the one before.
//!   Each writer closes before the other opens, outside the timed poll.
//!
//! As for the `reader` bench, judge a change by `net`, and compare two builds on one
//! pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../tests/common/net.rs"]
mod net;
#[path = "../tests/common/shard.rs"]
mod shard;
mod table;

use std::ops::RangeInclusive;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use block::Pool;
use env::tasks::Tasks;
use hub::home::Outcome;
use hub::writer::{self, Writer};
use hub::{Hub, Served, serve};
use shard::name;
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use table::Line;
use transport::stream::{Receiver, Sender};
use transport::{Address, Class, Transport};
use types::authority::Authority;
use types::channel;
use types::frame::{Draft, Form, Label, Path};
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::Protocol;
use wire::hub::{FromHome, Mode, Open, Reader, keys};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round.
const FRAMES: usize = 64;
const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// The frames of each session.
const RUN: usize = (WARMUP + ROUNDS) * FRAMES;
/// Samples per series of a `wide` frame.
const SAMPLES: usize = 128;
/// A ring that holds each frame of the run, as nothing frees a ring until #160.
const AREA: u64 = 16 * shard::AREA;
/// Past the commit of a write.
const SETTLE: Span = Span::from_nanos(20_000_000);
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
    TwoSets,
}

impl Shape {
    /// The samples of each series of a frame.
    fn samples(self) -> usize {
        match self {
            Self::Narrow | Self::TwoSets => 1,
            Self::Wide => SAMPLES,
        }
    }

    /// The key sets of the line's writers from `base`. The session reads each key.
    fn sets(self, base: u128) -> Vec<Vec<u128>> {
        let narrow = || std::iter::once(NARROW).chain(DATA).map(|key| base + key);
        match self {
            Self::Narrow => vec![narrow().collect()],
            Self::Wide => vec![vec![base + WIDE, base + WIDE_DATA]],
            Self::TwoSets => {
                let half = base + (DATA.end() - DATA.start()) / 2 + DATA.start();
                vec![
                    narrow().filter(|&key| key <= half).collect(),
                    narrow()
                        .filter(|&key| key == base + NARROW || key > half)
                        .collect(),
                ]
            }
        }
    }
}

/// The first key of the channels of line `at` of peer `turn`.
fn base(turn: usize, at: usize) -> u128 {
    let before: usize = PEERS[..turn].iter().map(|(_, specs)| specs.len()).sum();
    u128::try_from(before + at).expect("few lines") * LINE
}

/// One line: one session of a peer.
struct Spec {
    name: &'static str,
    mode: Mode,
    shape: Shape,
}

const LATEST: Mode = Mode::Latest;
const COMPLETE: Mode = Mode::Complete {
    limit_bytes: 1 << 40,
};

/// The lines of each peer, by the message limit of its transport.
const PEERS: [(usize, &[Spec]); 2] = [
    (
        1472,
        &[
            Spec {
                name: "narrow 1472",
                mode: LATEST,
                shape: Shape::Narrow,
            },
            Spec {
                name: "wide 1472",
                mode: LATEST,
                shape: Shape::Wide,
            },
            Spec {
                name: "complete 1472",
                mode: COMPLETE,
                shape: Shape::Narrow,
            },
            Spec {
                name: "two sets 1472",
                mode: LATEST,
                shape: Shape::TwoSets,
            },
        ],
    ),
    (
        1 << 16,
        &[
            Spec {
                name: "narrow 64k",
                mode: LATEST,
                shape: Shape::Narrow,
            },
            Spec {
                name: "wide 64k",
                mode: LATEST,
                shape: Shape::Wide,
            },
        ],
    ),
];

/// What the peers tell the home.
#[derive(Default)]
struct Shared {
    /// The peer whose turn it is to dial.
    turn: AtomicUsize,
    /// The sessions that the home opened.
    opened: AtomicUsize,
    /// The frames that the peers read in full.
    frames: AtomicUsize,
}

fn main() {
    let mut sim = sim::Sim::new(sim::Config {
        steps_max: u64::MAX,
        ..sim::Config::default()
    });
    let home = sim.node(sim::node::Config::default());
    let shared = Arc::new(Shared::default());
    let at = Address::Udp(std::net::SocketAddr::new(home.addresses()[0], net::PORT));
    for (turn, (message, _)) in PEERS.into_iter().enumerate() {
        let node = sim.node(sim::node::Config::default());
        let shared = Arc::clone(&shared);
        let config = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = node.clone();
        let main = move |tasks: Tasks| async move {
            peer(&own, &tasks, message, at, turn, &shared).await;
        };
        drop(node.shards().start(config, main).expect("starts"));
    }
    let (timer, lines) = sim
        .run_on(&home, move |node, tasks| bench(node, tasks, shared))
        .expect("the run ends");
    let title = format!("ns per frame over {ROUNDS} rounds of {FRAMES} frames");
    table::print(&title, &timer, &lines);
}

/// The home: serves each session of each peer in turn, and gives the `timer` line and
/// a line for each session.
async fn bench(
    node: sim::node::Node,
    tasks: Tasks,
    shared: Arc<Shared>,
) -> (Line, Vec<Line>) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
    let (hub, mut stamp) = create_hub(&node, tasks).await;
    let mut timer = Line::new("timer", FRAMES);
    let mut lines = Vec::new();
    for (turn, (_, specs)) in PEERS.into_iter().enumerate() {
        let (session, incoming) = net::accept(&transport).await;
        let mut first = Some(incoming);
        let remote = hub.link(session.clone());
        for (at, spec) in specs.iter().enumerate() {
            let incoming = match first.take() {
                Some(incoming) => incoming,
                None => net::stream(&session).await,
            };
            let mut serving = pin!(remote.serve(incoming));
            let mut line = Line::new(spec.name, FRAMES);
            let configs = configs(base(turn, at), spec.shape);
            let mut writer = hub.writer(configs[0].clone()).await.expect("opens");
            let opened = shared.opened.load(Ordering::Relaxed);
            while shared.opened.load(Ordering::Relaxed) == opened {
                poll(serving.as_mut());
                node.clock().sleep(STEP).await;
            }
            for round in 0..WARMUP + ROUNDS {
                let frames = shared.frames.load(Ordering::Relaxed) + FRAMES;
                match spec.mode {
                    Mode::Latest => {
                        for frame in 0..FRAMES {
                            if configs.len() > 1 {
                                drop(writer);
                                let config = configs[frame % configs.len()].clone();
                                writer = hub.writer(config).await.expect("opens");
                            }
                            let frame =
                                draft(&writer, spec.shape.samples(), &mut stamp);
                            write(&mut writer, frame);
                            timer.add(table::timed(&ALLOCATOR, || ()).1);
                            line.add(
                                table::timed(&ALLOCATOR, || poll(serving.as_mut())).1,
                            );
                        }
                    }
                    Mode::Complete { .. } => {
                        for _ in 0..FRAMES {
                            let frame =
                                draft(&writer, spec.shape.samples(), &mut stamp);
                            write(&mut writer, frame);
                            timer.add(table::timed(&ALLOCATOR, || ()).1);
                        }
                        node.clock().sleep(SETTLE).await;
                        line.add(table::timed(&ALLOCATOR, || poll(serving.as_mut())).1);
                    }
                }
                read(&node, &shared, frames).await;
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

/// Waits until the peers have read `frames` frames in full.
///
/// # Panics
///
/// When they read more.
async fn read(node: &sim::node::Node, shared: &Shared, frames: usize) {
    while shared.frames.load(Ordering::Relaxed) < frames {
        node.clock().sleep(STEP).await;
    }
    assert_eq!(
        shared.frames.load(Ordering::Relaxed),
        frames,
        "the peer reads each frame of the round once"
    );
}

/// The peer of `turn`: dials the home at `at` on its turn, reads each of its sessions
/// in turn until it has read `RUN` frames, then finishes it, and passes the turn on.
async fn peer(
    node: &sim::node::Node,
    tasks: &Tasks,
    message: usize,
    at: Address,
    turn: usize,
    shared: &Shared,
) {
    while shared.turn.load(Ordering::Relaxed) != turn {
        node.clock().sleep(STEP).await;
    }
    let pool = net::own_pool();
    let transport: Transport = net::transport(node, tasks, &pool, net::PEER, message);
    let session = transport
        .dial(net::public_key(&net::HOME), &[at])
        .await
        .expect("dials");
    for (line, spec) in PEERS[turn].1.iter().enumerate() {
        let class = match spec.mode {
            Mode::Latest => Class::Latest,
            Mode::Complete { .. } => Class::Complete,
        };
        let (mut sender, mut receiver) = session.open(class).await.expect("opens");
        send(&pool, &mut sender, &wire::header::encode(Protocol::Hub)).await;
        let mut keys = spec.shape.sets(base(turn, line)).concat();
        keys.sort_unstable();
        keys.dedup();
        let mut reader =
            open(&pool, &mut sender, &mut receiver, spec.mode, &keys).await;
        shared.opened.fetch_add(1, Ordering::Relaxed);
        let mut frames = 0;
        while frames < RUN {
            let message = receiver.recv().await.expect("receives").expect("a message");
            let decoded = reader.decode(&message).expect("a valid message");
            let ended = matches!(
                decoded,
                FromHome::Body { .. } | FromHome::Ends { last: true, .. }
            );
            if ended && reader.body().is_none() {
                frames += 1;
                shared.frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        sender.finish().expect("finishes");
        let finished = receiver.recv().await;
        assert!(
            matches!(finished, Ok(None)),
            "the home finishes: {finished:?}"
        );
    }
    session.close(transport::Code(0));
    node.clock().sleep(STEP).await;
    shared.turn.fetch_add(1, Ordering::Relaxed);
}

/// Sends the open of a session of `mode` on `keys`, and reads the reply.
async fn open(
    pool: &Pool,
    sender: &mut Sender,
    receiver: &mut Receiver,
    mode: Mode,
    keys: &[u128],
) -> Reader {
    let keys: Vec<_> = keys.iter().copied().map(channel::Key::from_u128).collect();
    let open = Open {
        mode,
        channels: u32::try_from(keys.len()).expect("the keys fit a u32"),
    };
    let mut out = vec![0; open.encoded_len()];
    open.encode(&mut out);
    send(pool, sender, &out).await;
    let mut out = vec![0; keys.len() * keys::LEN];
    keys::encode(&keys, &mut out);
    send(pool, sender, &out).await;
    let mut reader = Reader::new(&open);
    let opened = receiver.recv().await.expect("receives").expect("a reply");
    assert!(
        matches!(reader.decode(&opened), Ok(FromHome::Opened)),
        "the home opens the session"
    );
    reader
}

async fn send(pool: &Pool, sender: &mut Sender, bytes: &[u8]) {
    let mut block = pool.alloc(bytes.len()).expect("the pool has room");
    block.copy_from_slice(bytes);
    sender.send(block.freeze()).await.expect("sends");
}

/// One poll of `serving` with a waker that does nothing.
///
/// # Panics
///
/// When the session ends.
fn poll(serving: Pin<&mut impl Future<Output = Result<Served, serve::Error>>>) {
    let mut cx = Context::from_waker(Waker::noop());
    if let Poll::Ready(served) = serving.poll(&mut cx) {
        panic!("the session ended: {served:?}");
    }
}

/// A hub on a new shard whose ring holds the run, with the channels of each shape
/// defined, and the node's mesh time now.
async fn create_hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
    let (home, interner, now, time) = shard::shard(node, tasks.clone(), AREA).await;
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time,
        entropy: node.entropy(),
        mesh: None,
    });
    let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
    let mut definitions = Vec::new();
    let groups = PEERS.iter().enumerate().flat_map(|(turn, (_, specs))| {
        (0..specs.len()).flat_map(move |at| {
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
    for entry in 0..entries.len() {
        let bytes = draft.series_mut(entry).expect("the series is present");
        for (sample, stamp) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(first..) {
            sample.copy_from_slice(&stamp.to_le_bytes());
        }
    }
    draft.set_count(entries[0].group, u32::try_from(samples).expect("few"));
    draft
}

/// Writes `draft` as a live frame.
///
/// # Panics
///
/// When the home does not apply it.
fn write(writer: &mut Writer, draft: Draft) {
    let outcomes = writer
        .write(Label::Path(Path::Live), draft)
        .expect("the home takes it");
    assert!(
        matches!(outcomes, [Outcome::Applied { .. }]),
        "the home applies the frame: {outcomes:?}"
    );
}
