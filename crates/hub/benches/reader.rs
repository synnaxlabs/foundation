//! The per-frame cost of a `hub` reader, on one shard of a sim node. Run with
//! `cargo bench -p hub --bench reader`.
//!
//! Each round writes `FRAMES` frames of one sample on an index and one data channel,
//! and times six lines:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `write`, the control: one `Writer::write` of a frame whose draft is ready.
//! - `latest next`: one poll of a latest reader's `next` right after each write, which
//!   gives that frame before its commit.
//! - `complete next`: one poll of a complete reader's `next` after the round's commit,
//!   which gives a frame. Each but the first of a round also grants the credit of the
//!   frame before it.
//! - `complete grant`: the first poll of `next` on the drained complete reader, once a
//!   round: it grants the credit of the round's last frame, finds no frame, and gives
//!   `Pending`.
//! - `complete wait`: each later poll of `next` on the drained complete reader, which
//!   grants nothing and gives `Pending`.
//!
//! A `next` that gives `Pending` while a frame waits is the yield after a run of
//! frames: it is polled again, and only the poll that gives the frame is timed. Any
//! other result panics, so no figure holds a wait on the sim.
//!
//! The write reads the sim clock once, which costs less than an `os` read, so compare a
//! figure only with the control or with another build. The `timer` floor is more than
//! half of a poll's figure, so judge a change in a poll by `net`, its p50 less the
//! floor's. To compare two builds, run each several times in turn on one pinned core.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../tests/common/mod.rs"]
mod common;

use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use common::{SETTLE, name};
use hub::reader::{Mode, Reader};
use hub::writer;
use types::authority::Authority;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round. A round fits the window of a complete reader.
const FRAMES: usize = 64;
const WARMUP: usize = 20;
const ROUNDS: usize = 200;

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let lines = sim.run_on(&node, bench).expect("the run ends");
    print(&lines);
}

/// One line of the table.
struct Line {
    name: &'static str,
    /// The timed calls of each round.
    calls: u64,
    /// The ns and allocations of the round so far.
    round: (u64, u64),
    /// The ns per call of each timed round.
    nanos: Vec<u64>,
    /// The allocations of all timed rounds.
    allocations: u64,
}

impl Line {
    fn new(name: &'static str, calls: usize) -> Self {
        Self {
            name,
            calls: u64::try_from(calls).expect("few"),
            round: (0, 0),
            nanos: Vec::with_capacity(ROUNDS),
            allocations: 0,
        }
    }

    fn add(&mut self, (nanos, allocations): (u64, u64)) {
        self.round.0 += nanos;
        self.round.1 += allocations;
    }

    /// The ns per call of the round at `percent`.
    fn at(&self, percent: usize) -> u64 {
        let mut nanos = self.nanos.clone();
        nanos.sort_unstable();
        nanos[ROUNDS * percent / 100]
    }

    /// Ends a round, and keeps its figures when it is `timed`.
    fn close(&mut self, timed: bool) {
        let (nanos, allocations) = std::mem::take(&mut self.round);
        if timed {
            self.nanos.push(nanos / self.calls);
            self.allocations += allocations;
        }
    }
}

async fn bench(node: sim::node::Node, tasks: env::tasks::Tasks) -> [Line; 6] {
    let (hub, mut stamp) = common::hub(&node, tasks).await;
    let config = writer::Config {
        subject: name("bench"),
        authority: Authority(1),
        lease: None,
        channels: vec![name("value")],
    };
    let mut writer = hub.writer(config).await.expect("opens");
    let channels = [name("value")];
    let mut latest = hub.reader(&channels, Mode::Latest).await.expect("opens");
    let mut complete = hub.reader(&channels, Mode::Complete).await.expect("opens");
    let mut timer = Line::new("timer", FRAMES);
    let mut write = Line::new("write", FRAMES);
    let mut latest_next = Line::new("latest next", FRAMES);
    let mut complete_next = Line::new("complete next", FRAMES);
    let mut grant = Line::new("complete grant", 1);
    let mut wait = Line::new("complete wait", FRAMES - 1);
    for round in 0..WARMUP + ROUNDS {
        for _ in 0..FRAMES {
            let draft = common::draft(&writer, stamp);
            stamp += 1;
            timer.add(timed(|| ()));
            write.add(timed(|| common::write(&mut writer, draft)));
            latest_next.add(take(&mut latest));
        }
        node.clock().sleep(SETTLE).await;
        for _ in 0..FRAMES {
            complete_next.add(take(&mut complete));
        }
        grant.add(timed(|| pending(&mut complete)));
        for _ in 1..FRAMES {
            wait.add(timed(|| pending(&mut complete)));
        }
        let lines = [
            &mut timer,
            &mut write,
            &mut latest_next,
            &mut complete_next,
            &mut grant,
            &mut wait,
        ];
        for line in lines {
            line.close(round >= WARMUP);
        }
    }
    [timer, write, latest_next, complete_next, grant, wait]
}

/// The ns and allocations of the poll of `reader.next()` that gives the frame that
/// waits. A first `Pending`, the yield after a run of frames, is not counted.
///
/// # Panics
///
/// When no frame waits.
fn take(reader: &mut Reader) -> (u64, u64) {
    for _ in 0..2 {
        let ((ready, span), counted) = ALLOCATOR.count(|| clocked(|| poll(reader)));
        if ready {
            return (span, counted);
        }
    }
    panic!("a frame waits for the reader");
}

/// Polls `reader`, which has no frame waiting.
///
/// # Panics
///
/// When the poll gives a frame.
fn pending(reader: &mut Reader) {
    assert!(!poll(reader), "the reader has no frame left");
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
fn print(lines: &[Line]) {
    println!("ns per call over {ROUNDS} rounds of {FRAMES} frames");
    println!("pN: the round at percentile N");
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>9} {:>13}",
        "line", "p10", "p50", "p90", "net", "allocs/call"
    );
    let floor = lines[0].at(50);
    for line in lines {
        let allocations = per(line.allocations) / per(ROUNDS) / per(line.calls);
        println!(
            "{:<14} {:>9} {:>9} {:>9} {:>9} {allocations:>13.2}",
            line.name,
            line.at(10),
            line.at(50),
            line.at(90),
            line.at(50).saturating_sub(floor)
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
