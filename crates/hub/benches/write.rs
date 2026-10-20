//! The cost of one live `Writer::write` of a frame of `CHANNELS` data channels of one
//! sample type, one sample each, on one index. Run with
//! `cargo bench -p hub --bench write`.
//!
//! Each round is a new sim node and hub, so the ring never fills. It writes `FRAMES`
//! frames, and sleeps `SETTLE` after each `BATCH` of them. The 64 KiB record limit
//! of the test ring splits a `BATCH` into more than one commit. The draft of each
//! write is filled before it is timed. Compare a line only with another line or
//! another build, on one pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/node.rs"]
mod node;
mod table;

use std::collections::BTreeMap;

use common::{SETTLE, name};
use hub::writer::{self, Writer};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use table::Line;
use types::authority::Authority;
use types::channel;
use types::frame::{Draft, Form};
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
    for (label, scalar) in SCALARS {
        let mut line = Line::new(label, FRAMES);
        for round in 0..WARMUP + ROUNDS {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let (floor, figures) = sim
                .run_on(&node, move |node, tasks| run(node, tasks, scalar))
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

/// The figures of the timer and of each write of one round.
async fn run(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    scalar: Scalar,
) -> (Figures, Figures) {
    let (hub, now) = hub::testing::open(node::env(&node, tasks)).await;
    hub.set_definitions(&definitions(scalar));
    let config = writer::Config {
        subject: name("bench"),
        authority: Authority(1),
        lease: None,
        channels: (0..CHANNELS).map(|n| name(&format!("c{n}"))).collect(),
    };
    let mut writer = hub.writer(config).await.expect("opens");
    let (mut floor, mut figures) = (Vec::new(), Vec::new());
    for (frame, stamp) in (0..FRAMES).zip(now.nanos()..) {
        let draft = draft(&writer, scalar, stamp);
        floor.push(table::timed(&ALLOCATOR, || ()).1);
        figures.push(write(&mut writer, draft));
        if frame % BATCH == BATCH - 1 {
            node.clock().sleep(SETTLE).await;
        }
    }
    (floor, figures)
}

/// The figure of one write of `draft`. Its code stays apart from that of the open, so
/// a change of the open does not move it.
#[inline(never)]
fn write(writer: &mut Writer, draft: Draft) -> (u64, u64) {
    table::timed(&ALLOCATOR, || common::write(writer, draft)).1
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
