//! The per-frame cost of a `hub` write and read, on one shard of a sim node. Run with
//! `cargo bench -p hub --bench reader`.
//!
//! Each round writes `FRAMES` frames of one sample on an index and one data channel,
//! and times these lines:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `now`: one `Writer::now`, before each write.
//! - `first write`: the first `Writer::write` of a round, after the round before it
//!   committed.
//! - `write`, the control: each later `Writer::write` of a round but the last. The
//!   draft of each write is ready before it is timed, and the timed window, both clock
//!   reads and the write, is one call out of line, so the frame of the async bench
//!   does not change its instructions.
//! - `write wake`: the last `Writer::write` of a round. The latest reader waits for it
//!   with a counting waker, which the write wakes.
//! - `latest next`: one poll of a latest reader's `next` right after each write, which
//!   gives that frame before its commit.
//! - `latest ack`: for each frame that `latest next` gives, its `Received::position`
//!   and the `Reader::ack` of it, timed as two calls.
//! - `complete next`: one poll of a complete reader's `next` after the round's commit,
//!   which gives a frame. Each but the first of a round also grants the credit of the
//!   frame before it.
//! - `complete gap`: for each frame that `complete next` gives, its `Received::gap`,
//!   which is `None`.
//! - `complete ack`: for each frame that `complete next` gives, its
//!   `Received::position` and the `Reader::ack` of it, timed as two calls.
//! - `complete grant`: the first poll of `next` on the drained complete reader, once a
//!   round: it grants the credit of the round's last frame, finds no frame, and gives
//!   `Pending`.
//! - `complete wait`: each later poll of `next` on the drained complete reader, which
//!   grants nothing and gives `Pending`.
//! - `lost write`: after the rounds above, a read holds the oldest record of the ring,
//!   so no trim frees it, and the bench writes until the ring has no room. Then each
//!   `Writer::write` of its own rounds finds no room, gives `Outcome::Lost`, and wakes
//!   the latest reader with its frame.
//!
//! A `next` that gives `Pending` while a frame waits is the yield after a run of
//! frames: it is polled again, and only the poll that gives the frame is timed. Any
//! other result panics, so no figure holds a wait on the sim.
//!
//! The write reads the sim clock once, which costs less than an `os` read, so compare a
//! figure only with the control or with another build. The `timer` floor is a large
//! part of a poll's figure, so judge a change in a poll by `net`, its p50 less the
//! floor's. To compare two builds, run each several times in turn on one pinned core
//! whose SMT sibling is idle: a busy sibling doubles `write`. Last, it prints the
//! bytes of a `Received`, its parts, and a `Reader`.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/node.rs"]
mod node;
mod table;

use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use block::{Heap, Pool};

use common::{SETTLE, name, unnamed};
use hub::Hub;
use hub::home::Outcome;
use hub::reader::{Mode, Position, Reader, Received};
use hub::writer::{self, Writer};
use table::Line;
use types::authority::Authority;
use types::channel::Slot;
use types::frame::key_set::Interner;
use types::frame::{Draft, Label, Path, View};
use types::time::{Span, Stamp};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round. A round fits the window of a complete reader.
const FRAMES: usize = 64;
/// `WARMUP + ROUNDS` commits, under the 341 of `FRAMES` frames that the ring of
/// [`open`] holds.
const WARMUP: usize = 20;
const ROUNDS: usize = 200;
/// More rounds than fill the ring.
const FILL: usize = 1 << 10;

#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let (timer, lines) = sim.run_on(&node, bench).expect("the run ends");
    let title = format!("ns per call over {ROUNDS} rounds of {FRAMES} frames");
    table::print(&title, &timer, &lines);
    println!(
        "bytes: Received {}, Position {}, View {}, Reader {}",
        size_of::<Received<'_>>(),
        size_of::<Position>(),
        size_of::<View<'_>>(),
        size_of::<Reader>()
    );
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
async fn bench(node: sim::node::Node, tasks: env::tasks::Tasks) -> (Line, Vec<Line>) {
    let (hub, ring, mut stamp) = open(&node, tasks).await;
    let config = writer::Config {
        subject: name("bench"),
        authority: Authority(1),
        lease: None,
        channels: vec![name("value")],
    };
    let mut writer = hub.writer(config).await.expect("opens");
    let channels = ["value"];
    let latest = unnamed(&channels, Mode::Latest);
    let mut latest_reader = hub.reader(latest).await.expect("opens");
    let complete = unnamed(&channels, Mode::Complete);
    let mut complete_reader = hub.reader(complete).await.expect("opens");
    let count = Arc::new(Count::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut timer = Line::new("timer", FRAMES);
    let mut lines = [
        Line::new("now", FRAMES),
        Line::new("first write", 1),
        Line::new("write", FRAMES - 2),
        Line::new("write wake", 1),
        Line::new("latest next", FRAMES),
        Line::new("latest ack", FRAMES),
        Line::new("complete next", FRAMES),
        Line::new("complete gap", FRAMES),
        Line::new("complete ack", FRAMES),
        Line::new("complete grant", 1),
        Line::new("complete wait", FRAMES - 1),
    ];
    for round in 0..WARMUP + ROUNDS {
        let [now, first, control, wake, latest, latest_ack, complete @ ..] = &mut lines;
        for frame in 0..FRAMES {
            let draft = common::draft(&writer, stamp);
            stamp += 1;
            timer.add(table::timed(&ALLOCATOR, || ()).1);
            now.add(table::timed(&ALLOCATOR, || writer.now()).1);
            if frame == FRAMES - 1 {
                let polled = poll(&mut latest_reader, &waker);
                assert!(polled.is_none(), "the latest reader waits");
                assert_eq!(count.wakes(), round, "the poll wakes nothing");
            }
            let line = match frame {
                0 => &mut *first,
                _ if frame == FRAMES - 1 => &mut *wake,
                _ => &mut *control,
            };
            let (outcome, figures) = timed_write(&mut writer, draft);
            assert!(
                matches!(outcome, Outcome::Applied { .. }),
                "the home applies the frame: {outcome:?}"
            );
            line.add(figures);
            take_and_ack(&mut latest_reader, [latest, latest_ack], |_| ());
        }
        let woken = count.wakes();
        assert_eq!(woken, round + 1, "each round's last write wakes the reader");
        node.clock().sleep(SETTLE).await;
        drain(&mut complete_reader, complete);
        for line in std::iter::once(&mut timer).chain(&mut lines) {
            line.close(round >= WARMUP);
        }
    }
    let held = hold(&ring).await;
    fill(&node, &mut writer, &mut complete_reader, &mut stamp).await;
    let mut lines = Vec::from(lines);
    lines.push(lose(&mut writer, &mut latest_reader, stamp));
    drop(held);
    (timer, lines)
}

/// A hub as `common::hub` gives, a reader of the ring of its home, and the node's mesh
/// time now.
async fn open(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
) -> (Hub, buffer::Reader, i64) {
    let config = block::Config::new(1 << 23).expect("the budget fits");
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
        tasks: tasks.clone(),
        entropy: node.entropy(),
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let ring = buffer.reader();
    let home = home::Shard::open(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: home::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::SECOND,
        },
    })
    .await
    .expect("opens");
    let now = loop {
        if let Some(now) = home.now() {
            break now;
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    };
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time: mesh,
        entropy: node.entropy(),
        region: None,
    });
    hub.set_definitions(&common::definitions(&[]));
    (hub, ring, now.nanos())
}

/// A read of the live path of `time` from seq 0 on `ring`, polled once: it waits on
/// its file read, so no trim passes its record until it drops.
///
/// # Panics
///
/// When the read does not wait.
async fn hold(
    ring: &buffer::Reader,
) -> Pin<Box<impl Future<Output = Result<buffer::Read, buffer::Error>> + '_>> {
    let from = buffer::Mark::at(0);
    let mut read = Box::pin(ring.read(Slot::new(0), Path::Live, from, usize::MAX));
    let first = std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
    assert!(first.is_pending(), "the read waits on a file read");
    read
}

/// Takes and acks the `FRAMES` frames that wait for `reader`, a complete reader, then
/// polls it `FRAMES` times more, and adds the figures to the lines of `complete next`,
/// `complete gap`, `complete ack`, `complete grant`, and `complete wait`.
///
/// # Panics
///
/// When a frame has a gap.
fn drain(reader: &mut Reader, [next, gap, ack, grant, wait]: &mut [Line; 5]) {
    for _ in 0..FRAMES {
        take_and_ack(reader, [next, ack], |received| {
            let (lost, figures) = table::timed(&ALLOCATOR, || received.gap());
            assert_eq!(lost, None, "no seq was lost");
            gap.add(figures);
        });
    }
    grant.add(table::timed(&ALLOCATOR, || pending(reader)).1);
    for _ in 1..FRAMES {
        wait.add(table::timed(&ALLOCATOR, || pending(reader)).1);
    }
}

/// The `lost write` line, from `stamp`, once the ring has no room.
fn lose(writer: &mut Writer, latest: &mut Reader, mut stamp: i64) -> Line {
    let mut line = Line::new("lost write", FRAMES);
    for round in 0..WARMUP + ROUNDS {
        for _ in 0..FRAMES {
            let draft = common::draft(writer, stamp);
            stamp += 1;
            let (outcome, figures) = timed_write(writer, draft);
            assert!(
                matches!(outcome, Outcome::Lost { .. }),
                "the ring has no room for the frame: {outcome:?}"
            );
            line.add(figures);
            take(latest, |_| ());
        }
        line.close(round >= WARMUP);
    }
    line
}

/// Writes rounds of frames, each with its commit, until the ring has no room for one.
///
/// # Panics
///
/// When `FILL` rounds leave room.
async fn fill(
    node: &sim::node::Node,
    writer: &mut Writer,
    complete: &mut Reader,
    stamp: &mut i64,
) {
    for _ in 0..FILL {
        for _ in 0..FRAMES {
            let draft = common::draft(writer, *stamp);
            *stamp += 1;
            if let Outcome::Lost { .. } = write(writer, draft) {
                return;
            }
        }
        node.clock().sleep(SETTLE).await;
        while poll(complete, Waker::noop()).is_some()
            || poll(complete, Waker::noop()).is_some()
        {}
    }
    panic!("the ring fills");
}

/// The ns and allocations of the poll of `reader.next()` that gives the frame that
/// waits, and what `then` gives of the frame, untimed. A first `Pending`, the yield
/// after a run of frames, is not counted.
///
/// # Panics
///
/// When no frame waits.
fn take<T>(
    reader: &mut Reader,
    then: impl FnOnce(Received<'_>) -> T,
) -> ((u64, u64), T) {
    for _ in 0..2 {
        let (polled, figures) =
            table::timed(&ALLOCATOR, || poll(reader, Waker::noop()));
        if let Some(received) = polled {
            return (figures, then(received));
        }
    }
    panic!("a frame waits for the reader");
}

/// Takes the frame that waits for `reader` as [`take`] does, gives it to `then`, adds
/// the figures of its poll to `next`, and to `ack` those of its position and of the
/// ack of it.
fn take_and_ack(
    reader: &mut Reader,
    [next, ack]: [&mut Line; 2],
    then: impl FnOnce(&Received<'_>),
) {
    let (figures, position) = take(reader, |received| {
        then(&received);
        let (position, figures) = table::timed(&ALLOCATOR, || received.position());
        ack.add(figures);
        position
    });
    next.add(figures);
    ack.add(table::timed(&ALLOCATOR, || reader.ack(position)).1);
}

/// Writes `draft` as a live frame, and gives its outcome.
///
/// # Panics
///
/// When the write fails or gives other than one outcome.
fn write(writer: &mut Writer, draft: Draft) -> Outcome {
    outcome(writer.write(Label::Path(Path::Live), draft))
}

/// Writes `draft` as [`write`] does, and gives its outcome with the ns and allocations
/// of the write. Both clock reads and the write are one call out of line, so the frame
/// of the async `bench` does not change their instructions.
///
/// # Panics
///
/// When the write fails or gives other than one outcome.
#[inline(never)]
fn timed_write(writer: &mut Writer, draft: Draft) -> (Outcome, (u64, u64)) {
    let (written, figures) =
        table::timed(&ALLOCATOR, || writer.write(Label::Path(Path::Live), draft));
    (outcome(written), figures)
}

/// The one outcome of a write.
///
/// # Panics
///
/// When the write fails or gives other than one outcome.
fn outcome(written: Result<&[Outcome], writer::Failure>) -> Outcome {
    match written {
        Ok([outcome]) => outcome.clone(),
        written => panic!("the write gives one outcome: {written:?}"),
    }
}

/// Polls `reader`, which has no frame waiting.
///
/// # Panics
///
/// When the poll gives a frame.
fn pending(reader: &mut Reader) {
    let polled = poll(reader, Waker::noop());
    assert!(polled.is_none(), "the reader has no frame left");
}

/// One poll of `reader.next()` with `waker`: the frame it gives, or `None` for
/// `Pending`.
///
/// # Panics
///
/// When the reader ends.
fn poll<'a>(reader: &'a mut Reader, waker: &Waker) -> Option<Received<'a>> {
    let mut cx = Context::from_waker(waker);
    match pin!(reader.next()).poll(&mut cx) {
        Poll::Ready(Ok(received)) => Some(received),
        Poll::Ready(Err(ended)) => panic!("the reader ended: {ended:?}"),
        Poll::Pending => None,
    }
}
