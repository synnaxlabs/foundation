//! The per-frame cost of a `hub` reader, on one shard of a sim node. Run with
//! `cargo bench -p hub --bench reader`.
//!
//! Each round writes `FRAMES` frames of one sample on an index and one data channel,
//! and times four lines, each per frame of the round:
//!
//! - `write`, the control: one `Writer::write` of a frame whose draft is ready.
//! - `latest next`: one poll of a latest reader's `next` right after each write, which
//!   gives that frame before its commit.
//! - `complete next`: one poll of a complete reader's `next` after the round's commit,
//!   which gives a frame and grants the credit of the frame before it.
//! - `complete wait`: one poll of `next` on the drained complete reader, which grants
//!   its credit, finds no frame, and gives `Pending`.
//!
//! A `next` that gives `Pending` while a frame waits is the yield after a run of
//! frames: it is polled again, and only the poll that gives the frame is timed. Any
//! other result panics, so no number holds a wait on the sim. To compare two builds,
//! run each several times in turn on one pinned core and compare p10 and p50 with the
//! control.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use block::{Heap, Pool};
use hub::reader::{Mode, Reader};
use hub::writer::{self, Writer};
use hub::{Channel, Hub};
use types::authority::Authority;
use types::channel;
use types::frame::key_set::Interner;
use types::frame::{Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round. A round fits the window of a complete reader.
const FRAMES: usize = 64;
/// Rounds before the timed rounds.
const WARMUP: usize = 20;
/// Timed rounds.
const ROUNDS: usize = 200;
const COMMIT: Span = Span::from_nanos(10_000_000);
/// Past the commit of a write.
const SETTLE: Span = Span::from_nanos(20_000_000);
const LIMITS: home::order::Limits = home::order::Limits {
    earliest: Stamp::from_nanos(1),
    ahead: Span::from_nanos(1_000_000_000),
};
const LINES: [&str; 4] = ["write", "latest next", "complete next", "complete wait"];

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let measured = sim.run_on(&node, bench).expect("the run ends");
    print(&measured);
}

/// For each line, the ns per frame of each timed round, and the allocations of all
/// timed rounds.
struct Measured {
    nanos: [Vec<u64>; 4],
    allocations: [u64; 4],
}

async fn bench(node: sim::node::Node, tasks: env::tasks::Tasks) -> Measured {
    let (hub, mesh) = hub(&node, tasks).await;
    let mut writer = hub.writer(writer_config()).await.expect("opens");
    let channels = [name("value")];
    let mut latest = hub.reader(&channels, Mode::Latest).await.expect("opens");
    let mut complete = hub.reader(&channels, Mode::Complete).await.expect("opens");
    let mut measured = Measured {
        nanos: Default::default(),
        allocations: [0; 4],
    };
    for round in 0..WARMUP + ROUNDS {
        let (mut nanos, mut allocations) = ([0; 4], [0; 4]);
        let mut add = |line: usize, (span, counted): (u64, u64)| {
            nanos[line] += span;
            allocations[line] += counted;
        };
        let now = now(&mesh);
        for n in 0..FRAMES {
            let draft = draft(&writer, now + i64::try_from(n).expect("few"));
            add(0, timed(|| write(&mut writer, draft)));
            add(1, take(&mut latest));
        }
        node.clock().sleep(SETTLE).await;
        for _ in 0..FRAMES {
            add(2, take(&mut complete));
        }
        for _ in 0..FRAMES {
            add(
                3,
                timed(|| assert!(!poll(&mut complete), "the reader has no frame left")),
            );
        }
        if round >= WARMUP {
            for line in 0..LINES.len() {
                let frames = u64::try_from(FRAMES).expect("few");
                measured.nanos[line].push(nanos[line] / frames);
                measured.allocations[line] += allocations[line];
            }
        }
    }
    measured
}

/// A hub on a new ring of `node`, with `time` and `value` on it, and its mesh clock,
/// once the node has mesh time.
async fn hub(node: &sim::node::Node, tasks: env::tasks::Tasks) -> (Hub, clock::Reader) {
    let config = block::Config { budget: 1 << 24 };
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (unsynced, mesh) = clock::Clock::new(node.clock());
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool,
        clock: node.clock(),
        tasks: tasks.clone(),
        entropy: node.entropy(),
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: COMMIT,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let home = home::Shard::new(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: LIMITS,
    });
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks: tasks.clone(),
    });
    for (key, channel, scalar) in
        [(1, "time", Scalar::Stamp), (2, "value", Scalar::I64)]
    {
        hub.define(Channel {
            key: channel::Key::from_u128(key),
            name: name(channel),
            data_type: Type::Scalar(scalar),
            index: channel::Key::from_u128(1),
        });
    }
    let wall = node.wall();
    tasks.spawn(async move { unsynced.run(wall).await });
    while mesh.now().mesh.is_none() {
        node.clock().sleep(Span::from_nanos(1)).await;
    }
    (hub, mesh)
}

fn writer_config() -> writer::Config {
    writer::Config {
        subject: name("bench"),
        authority: Authority(1),
        lease: None,
        channels: vec![name("value")],
    }
}

fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// Mesh time now: the midpoint of the clock's interval.
fn now(mesh: &clock::Reader) -> i64 {
    let now = mesh.now().mesh.expect("the node has mesh time");
    now.earliest.nanos().midpoint(now.latest.nanos())
}

/// A frame of one sample at `stamp` on `time` and `value`.
fn draft(writer: &Writer, stamp: i64) -> types::frame::Draft {
    let set = writer.set();
    let entry = |key| {
        let key = channel::Key::from_u128(key);
        set.entries()
            .iter()
            .position(|entry| entry.key == key)
            .expect("the key set holds the channel")
    };
    let (time, value) = (entry(1), entry(2));
    let mut series = [(time, 8), (value, 8)];
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for (entry, sample) in [(time, stamp), (value, stamp)] {
        let bytes = draft.series_mut(entry).expect("the series is present");
        bytes.copy_from_slice(&sample.to_le_bytes());
    }
    draft.set_count(set.entries()[time].group, 1);
    draft
}

fn write(writer: &mut Writer, draft: types::frame::Draft) {
    writer
        .write(Label::Path(Path::Live), draft)
        .expect("the home takes it");
}

/// The ns and allocations of the poll of `reader.next()` that gives the frame that
/// waits. A poll that gives the `Pending` of a yield after a run of frames is not
/// counted.
fn take(reader: &mut Reader) -> (u64, u64) {
    loop {
        let ((ready, span), counted) = ALLOCATOR.count(|| clocked(|| poll(reader)));
        if ready {
            return (span, counted);
        }
    }
}

/// One poll of `reader.next()`: `true` for a frame, `false` for `Pending`.
///
/// # Panics
///
/// When the reader ends.
fn poll(reader: &mut Reader) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    match pin!(reader.next()).poll(&mut cx) {
        Poll::Ready(Ok(_)) => true,
        Poll::Ready(Err(ended)) => panic!("the reader ended: {ended:?}"),
        Poll::Pending => false,
    }
}

/// The ns that `f` takes, and the allocations it makes.
fn timed(f: impl FnOnce()) -> (u64, u64) {
    let (((), span), counted) = ALLOCATOR.count(|| clocked(f));
    (span, counted)
}

/// The result of `f`, and the ns it takes.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn clocked<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = Instant::now();
    let value = f();
    (value, nanos(Instant::now().duration_since(start)))
}

fn nanos(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).expect("a poll takes under 2^64 ns")
}

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(measured: &Measured) {
    println!("ns per frame over {ROUNDS} rounds of {FRAMES} frames");
    println!("pN: the round at percentile N");
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>13}",
        "line", "p10", "p50", "p90", "allocs/frame"
    );
    for (line, name) in LINES.iter().enumerate() {
        let mut nanos = measured.nanos[line].clone();
        nanos.sort_unstable();
        let at = |percent: usize| nanos[ROUNDS * percent / 100];
        let allocations = per(measured.allocations[line]) / per(ROUNDS * FRAMES);
        println!(
            "{name:<14} {:>9} {:>9} {:>9} {allocations:>13.2}",
            at(10),
            at(50),
            at(90)
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
