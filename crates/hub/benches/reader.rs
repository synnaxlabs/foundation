//! The per-frame cost of a `hub` write and read, on one shard of a sim node. Run with
//! `cargo bench -p hub --bench reader`.
//!
//! Each round writes `FRAMES` frames of one sample on an index and one data channel,
//! and times these lines:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `first write`: the first `Writer::write` of a round, after the round before it
//!   committed. It wakes the commit task.
//! - `write`, the control: each later `Writer::write` of a round but the last. The
//!   draft of each write is ready before it is timed.
//! - `write wake`: the last `Writer::write` of a round. The latest reader waits for it
//!   with a counting waker, which the write wakes.
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
//! figure only with the control or with another build. The `timer` floor is a large
//! part of a poll's figure, so judge a change in a poll by `net`, its p50 less the
//! floor's. To compare two builds, run each several times in turn on one pinned core
//! whose SMT sibling is idle: a busy sibling doubles `write`.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/shard.rs"]
mod shard;
mod table;

use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use common::SETTLE;
use hub::reader::{Mode, Reader};
use hub::writer;
use shard::name;
use table::Line;
use types::authority::Authority;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round. A round fits the window of a complete reader.
const FRAMES: usize = 64;
/// `WARMUP + ROUNDS` commits, under the 341 of `FRAMES` frames that a ring of
/// `shard::AREA` holds.
const WARMUP: usize = 20;
const ROUNDS: usize = 200;

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let (timer, lines) = sim.run_on(&node, bench).expect("the run ends");
    let title = format!("ns per call over {ROUNDS} rounds of {FRAMES} frames");
    table::print(&title, &timer, &lines);
}

/// A waker that counts its wakes.
#[derive(Default)]
struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl Count {
    fn wakes(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// The `timer` line and the lines that it is the floor of.
async fn bench(node: sim::node::Node, tasks: env::tasks::Tasks) -> (Line, [Line; 7]) {
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
    let count = Arc::new(Count::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut timer = Line::new("timer", FRAMES);
    let mut lines = [
        Line::new("first write", 1),
        Line::new("write", FRAMES - 2),
        Line::new("write wake", 1),
        Line::new("latest next", FRAMES),
        Line::new("complete next", FRAMES),
        Line::new("complete grant", 1),
        Line::new("complete wait", FRAMES - 1),
    ];
    for round in 0..WARMUP + ROUNDS {
        let [first, write, wake, latest_next, complete_next, grant, wait] = &mut lines;
        for frame in 0..FRAMES {
            let draft = common::draft(&writer, stamp);
            stamp += 1;
            timer.add(table::timed(&ALLOCATOR, || ()).1);
            if frame == FRAMES - 1 {
                assert!(!poll(&mut latest, &waker), "the latest reader waits");
                assert_eq!(count.wakes(), round, "the poll wakes nothing");
            }
            let line = match frame {
                0 => &mut *first,
                _ if frame == FRAMES - 1 => &mut *wake,
                _ => &mut *write,
            };
            line.add(table::timed(&ALLOCATOR, || common::write(&mut writer, draft)).1);
            latest_next.add(take(&mut latest));
        }
        let woken = count.wakes();
        assert_eq!(woken, round + 1, "each round's last write wakes the reader");
        node.clock().sleep(SETTLE).await;
        for _ in 0..FRAMES {
            complete_next.add(take(&mut complete));
        }
        grant.add(table::timed(&ALLOCATOR, || pending(&mut complete)).1);
        for _ in 1..FRAMES {
            wait.add(table::timed(&ALLOCATOR, || pending(&mut complete)).1);
        }
        for line in std::iter::once(&mut timer).chain(&mut lines) {
            line.close(round >= WARMUP);
        }
    }
    (timer, lines)
}

/// The ns and allocations of the poll of `reader.next()` that gives the frame that
/// waits. A first `Pending`, the yield after a run of frames, is not counted.
///
/// # Panics
///
/// When no frame waits.
fn take(reader: &mut Reader) -> (u64, u64) {
    for _ in 0..2 {
        let (ready, figures) = table::timed(&ALLOCATOR, || poll(reader, Waker::noop()));
        if ready {
            return figures;
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
    assert!(!poll(reader, Waker::noop()), "the reader has no frame left");
}

/// One poll of `reader.next()` with `waker`: `true` for a frame, `false` for
/// `Pending`.
///
/// # Panics
///
/// When the reader ends.
fn poll(reader: &mut Reader, waker: &Waker) -> bool {
    let mut cx = Context::from_waker(waker);
    match pin!(reader.next()).poll(&mut cx) {
        Poll::Ready(Ok(_)) => true,
        Poll::Ready(Err(ended)) => panic!("the reader ended: {ended:?}"),
        Poll::Pending => false,
    }
}
