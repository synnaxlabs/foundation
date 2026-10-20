//! The cost of one live `Writer::write` of a frame of `CHANNELS` data channels of one
//! sample type, one sample each, on one index. Run with
//! `cargo bench -p hub --bench write`.
//!
//! Each round is a new sim node and hub, so the ring never fills. It writes `FRAMES`
//! frames, and sleeps `SETTLE` after each `BATCH` of them. The 64 KiB record limit
//! of the test ring splits a `BATCH` into more than one commit. The draft of each
//! write is filled before it is timed. The line `f64 lost handoff` first fills the
//! ring, then a writer of a higher authority takes control, and its handoff finds no
//! room: each of its writes checks the size of its frame before the room, and gives
//! `Outcome::Lost`. Compare a line only with another line or another build, on one
//! pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/node.rs"]
mod node;
mod table;

use std::collections::BTreeMap;

use common::{SETTLE, name};
use hub::home::Outcome;
use hub::writer::{self, Writer};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use table::Line;
use types::authority::Authority;
use types::channel;
use types::frame::{Draft, Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Data channels in each frame.
const CHANNELS: usize = 256;
/// Frames per round.
const FRAMES: usize = 64;
/// Frames between two sleeps of `SETTLE`.
const BATCH: usize = 16;
const WARMUP: usize = 5;
const ROUNDS: usize = 50;
/// Most frames that fill the ring.
const FILL: usize = 1 << 16;

const SCALARS: [(&str, Scalar); 6] = [
    ("f64", Scalar::F64),
    ("i64", Scalar::I64),
    ("f32", Scalar::F32),
    ("i32", Scalar::I32),
    ("i16", Scalar::I16),
    ("bool", Scalar::Bool),
];

fn main() {
    let mut timer = Line::new("timer", FRAMES);
    let mut lines: Vec<Line> = Vec::new();
    let lost = ("f64 lost handoff", Scalar::F64, true);
    let runs = SCALARS.map(|(label, scalar)| (label, scalar, false));
    for (label, scalar, lost) in runs.into_iter().chain([lost]) {
        let mut line = Line::new(label, FRAMES);
        for round in 0..WARMUP + ROUNDS {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let (floor, figures) = sim
                .run_on(&node, move |node, tasks| run(node, tasks, scalar, lost))
                .expect("the run ends");
            for (floor, figures) in floor.into_iter().zip(figures) {
                timer.add(floor);
                line.add(figures);
            }
            timer.close(round >= WARMUP);
            line.close(round >= WARMUP);
        }
        lines.push(line);
    }
    let title = format!(
        "ns per write of {CHANNELS} channels, one sample each, over {ROUNDS} rounds \
         of {FRAMES} frames"
    );
    table::print(&title, &timer, &lines);
}

type Figures = Vec<(u64, u64)>;

/// The figures of the timer and of each write of one round. When `lost`, each write
/// is of a writer whose handoff finds no room in the filled ring.
async fn run(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    scalar: Scalar,
    lost: bool,
) -> (Figures, Figures) {
    let (hub, now) = hub::testing::open(node::env(&node, tasks)).await;
    hub.set_definitions(&definitions(scalar));
    let mut writer = hub.writer(config("bench", 1)).await.expect("opens");
    let mut stamps = now.nanos()..;
    let mut taker = None;
    if lost {
        fill(&node, &mut writer, scalar, &mut stamps).await;
        taker = Some(hub.writer(config("taker", 2)).await.expect("opens"));
    }
    let (mut floor, mut figures) = (Vec::new(), Vec::new());
    for (frame, stamp) in (0..FRAMES).zip(stamps) {
        floor.push(table::timed(&ALLOCATOR, || ()).1);
        if let Some(taker) = &mut taker {
            let draft = draft(taker, scalar, stamp);
            let (written, figure) = table::timed(&ALLOCATOR, || {
                let outcomes = taker.write(Label::Path(Path::Live), draft);
                matches!(outcomes, Ok([Outcome::Lost { .. }]))
            });
            assert!(written, "the ring has no room for the frame");
            figures.push(figure);
        } else {
            let draft = draft(&writer, scalar, stamp);
            figures
                .push(table::timed(&ALLOCATOR, || common::write(&mut writer, draft)).1);
        }
        if frame % BATCH == BATCH - 1 {
            node.clock().sleep(SETTLE).await;
        }
    }
    (floor, figures)
}

/// The config of a writer of `c0` to `c255`.
fn config(subject: &str, authority: u8) -> writer::Config {
    writer::Config {
        subject: name(subject),
        authority: Authority(authority),
        lease: None,
        channels: (0..CHANNELS).map(|n| name(&format!("c{n}"))).collect(),
    }
}

/// Writes frames of `writer` at `stamps`, with a sleep of `SETTLE` after each `BATCH`,
/// until the ring has no room for one, as nothing frees it until #160.
///
/// # Panics
///
/// When `FILL` frames leave room.
async fn fill(
    node: &sim::node::Node,
    writer: &mut Writer,
    scalar: Scalar,
    stamps: &mut impl Iterator<Item = i64>,
) {
    for (frame, stamp) in (0..FILL).zip(stamps) {
        let draft = draft(writer, scalar, stamp);
        match writer.write(Label::Path(Path::Live), draft) {
            Ok([Outcome::Lost { .. }]) => return,
            Ok([Outcome::Applied { .. }]) => {}
            written => panic!("the write gives one outcome: {written:?}"),
        }
        if frame % BATCH == BATCH - 1 {
            node.clock().sleep(SETTLE).await;
        }
    }
    panic!("the ring fills");
}

/// `time` with key 1, and `c0` to `c255`, from key 2 on, each a channel of `scalar` on
/// `time`.
fn definitions(scalar: Scalar) -> BTreeMap<Name, Definition> {
    let time = Channel {
        key: channel::Key::from_u128(1),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    };
    let index = time.key;
    let sample = DataType::Sample(Type::Scalar(scalar));
    let data = |n: usize| {
        let key = u128::try_from(n).expect("few") + 2;
        let data = Data::new(index, None, sample.clone(), None).expect("no unit");
        Definition::Channel(Channel {
            key: channel::Key::from_u128(key),
            kind: Kind::Data(data),
        })
    };
    let mut definitions = BTreeMap::from([(name("time"), Definition::Channel(time))]);
    definitions.extend((0..CHANNELS).map(|n| (name(&format!("c{n}")), data(n))));
    definitions
}

/// A raw frame of one sample at `stamp` on `time` and on each data channel, whose
/// sample is the low bytes of its entry.
fn draft(writer: &Writer, scalar: Scalar, stamp: i64) -> Draft {
    let set = writer.set();
    let width = scalar.width();
    let series: Vec<(usize, usize)> = (0..set.entries().len())
        .map(|entry| {
            let index = set.index(entry) == entry;
            (entry, if index { 8 } else { width })
        })
        .collect();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    let mut group = None;
    for (entry, bytes) in draft.iter_mut() {
        if set.index(entry) == entry {
            bytes.copy_from_slice(&stamp.to_le_bytes());
            group = Some(set.entries()[entry].group);
        } else {
            let value = u64::try_from(entry).expect("few") % 2;
            bytes.copy_from_slice(&value.to_le_bytes()[..width]);
        }
    }
    draft.set_count(group.expect("the frame holds its index"), 1);
    draft
}
