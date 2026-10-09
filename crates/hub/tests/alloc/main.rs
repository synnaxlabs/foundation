//! A write and `next` make no heap allocation once the hub has taken a few frames: for
//! a complete reader when frames wait for it and when it waits for them, and for latest
//! readers that the write wakes, also two writes in one commit and readers replaced.
//! The commit task is not counted. The home's `woken` after a commit, which that task
//! calls, is counted on its own: it makes none once its keys are sized, while a
//! complete reader takes each frame, and when a complete reader falls behind again to
//! a depth it reached before. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/mod.rs"]
mod common;
#[path = "../common/node.rs"]
mod node;
#[path = "../common/woken.rs"]
mod woken;

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use common::{SETTLE, hub, name};
use home::reader::Next;
use home::reader::complete::Charge;
use hub::Hub;
use hub::reader::{Mode, Reader};
use hub::writer::{self, Writer};
use types::authority::Authority;
use woken::Woken;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames that warm the hub up: its vectors reach their size.
const WARM: i64 = 4;
/// Frames whose write and read are counted. More than the streak of `next`, so a
/// count covers its yield.
const COUNTED: i64 = 300;

/// Writes one sample to `time` and to `value`, with the allocations the write made.
fn write(writer: &mut Writer, stamp: i64) -> u64 {
    ALLOCATOR
        .count(|| common::write(writer, common::draft(writer, stamp)))
        .1
}

/// Takes the next frame of `reader`, which waits for it, with the allocations the
/// polls made. A first `Pending` is the yield of `next`.
fn read(reader: &mut Reader) -> u64 {
    let (taken, allocations) = ALLOCATOR.count(|| {
        let context = &mut Context::from_waker(Waker::noop());
        (0..2).any(|_| match pin!(reader.next()).poll(context) {
            Poll::Ready(received) => received.is_ok(),
            Poll::Pending => false,
        })
    });
    assert!(taken, "a committed frame waits for the reader");
    allocations
}

/// Polls `reader`, which has no frame waiting, once, with the allocations the poll
/// made. A `Pending` ends the streak of `next`, so the poll never yields.
fn wait(reader: &mut Reader) -> u64 {
    let (waited, allocations) = ALLOCATOR.count(|| {
        let context = &mut Context::from_waker(Waker::noop());
        pin!(reader.next()).poll(context).is_pending()
    });
    assert!(waited, "no frame waits for the reader");
    allocations
}

/// Writes that each wake five latest readers, `latest` and four more it opens, from
/// `now`: two writes in one commit after each `WARM` writes that each have a commit.
async fn five_latest(
    hub: &Hub,
    node: &sim::node::Node,
    writer: &mut Writer,
    latest: Reader,
    mut now: i64,
) {
    let mut latests = vec![latest];
    for _ in 0..4 {
        let latest = hub.reader(&[name("value")], Mode::Latest).await;
        latests.push(latest.expect("opens"));
    }
    for latest in &mut latests[1..] {
        read(latest);
    }
    for n in 0..3 * (WARM + 2) {
        let waited: u64 = latests.iter_mut().map(wait).sum();
        let written = write(writer, now);
        now += 1;
        let taken: u64 = latests.iter_mut().map(read).sum();
        if n % (WARM + 2) <= WARM {
            node.clock().sleep(SETTLE).await;
        }
        if n >= WARM {
            assert_eq!((waited, written, taken), (0, 0, 0), "five latest {n}");
        }
    }
}

/// Writes that each wake twelve latest readers from `now`, then one more after each
/// reader is replaced by one that takes the newest frame at open.
async fn replaced(
    hub: &Hub,
    node: &sim::node::Node,
    writer: &mut Writer,
    mut now: i64,
) {
    let mut latests = Vec::new();
    for _ in 0..12 {
        let latest = hub.reader(&[name("value")], Mode::Latest).await;
        latests.push(latest.expect("opens"));
    }
    for latest in &mut latests {
        read(latest);
    }
    // Keep this wait: a commit wake here takes any key put at open, so the warm writes
    // size the keys to twelve, and a key put at open shows in the last write.
    node.clock().sleep(SETTLE).await;
    for n in 0..2 * WARM {
        let waited: u64 = latests.iter_mut().map(wait).sum();
        let written = write(writer, now);
        now += 1;
        let taken: u64 = latests.iter_mut().map(read).sum();
        node.clock().sleep(SETTLE).await;
        if n >= WARM {
            assert_eq!((waited, written, taken), (0, 0, 0), "twelve latest {n}");
        }
    }
    for latest in &mut latests {
        let replaced = hub.reader(&[name("value")], Mode::Latest).await;
        *latest = replaced.expect("opens");
        read(latest);
    }
    let waited: u64 = latests.iter_mut().map(wait).sum();
    let written = write(writer, now);
    let taken: u64 = latests.iter_mut().map(read).sum();
    assert_eq!((waited, written, taken), (0, 0, 0), "replaced");
}

/// Writes a frame, waits for its commit, and calls `woken`. Returns the number of keys
/// it gave and the allocations it made.
async fn commit(woken: &mut Woken) -> (usize, u64) {
    woken.commit().await;
    let ((), allocations) = ALLOCATOR.count(|| woken.shard.woken(&mut woken.keys));
    (woken.keys.len(), allocations)
}

/// The home's `woken` with one complete reader that takes each frame: each call
/// after the first names the reader and allocates nothing.
fn woken_of_a_reader_that_takes_each_frame() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let mut calls = Vec::new();
        for n in 0..20 {
            calls.push(commit(&mut woken).await);
            assert!(
                matches!(woken.shard.take(woken.readers[0]), Next::Frame(_)),
                "frame {n} waits for the reader"
            );
        }
        assert_eq!(
            calls[1..],
            [(1, 0); 19],
            "(keys given, allocations) of each call after the first: {calls:?}"
        );
    })
    .expect("the run ends");
}

/// The home's `woken` with one complete reader that falls 64 frames behind, takes
/// each, then falls 64 frames behind again: no call of the second lag allocates.
fn woken_of_a_reader_that_falls_behind_again() {
    /// Deep enough that the waiting list of the reader grows more than once.
    const DEPTH: usize = 64;
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let mut lags = Vec::new();
        for lag in 0..2 {
            let mut calls = Vec::new();
            for _ in 0..DEPTH {
                calls.push(commit(&mut woken).await);
            }
            let reader = woken.readers[0];
            let taken = std::iter::from_fn(|| match woken.shard.take(reader) {
                Next::Frame(frame) => Some(frame),
                Next::Empty => None,
                Next::Behind => panic!("lag {lag}: the reader is behind"),
            })
            .count();
            assert_eq!(taken, DEPTH, "lag {lag}");
            lags.push(calls);
        }
        let grown = lags[0][1..]
            .iter()
            .filter(|&&(_, allocations)| allocations > 0);
        assert!(grown.count() > 1, "the first lag grows the list: {lags:?}");
        let mut expected = [(0, 0); DEPTH];
        expected[0] = (1, 0);
        assert_eq!(
            lags[1], expected,
            "(keys given, allocations) of each call of the second lag: {lags:?}"
        );
    })
    .expect("the run ends");
}

/// The home's `woken` with a reader that takes each frame and one whose credit covers
/// one frame at a time: once a frame of the second waits for credit, as many frames
/// wait for it as waited before, so the call allocates nothing.
fn woken_of_a_reader_whose_frame_waits_for_credit() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let slot = woken.set.entries()[woken.set.groups()[0]].slot;
        let limited = woken.shard.open_complete(slot, 1, Charge::Whole);
        let mut calls = Vec::new();
        let mut spent = 0;
        for n in 0..6 {
            if n < 5 {
                woken.shard.grant(limited, spent + 1);
            }
            calls.push(commit(&mut woken).await);
            assert!(
                matches!(woken.shard.take(woken.readers[0]), Next::Frame(_)),
                "frame {n} waits for the reader"
            );
            if n < 5 {
                let Next::Frame(frame) = woken.shard.take(limited.into()) else {
                    panic!("frame {n} waits for the limited reader");
                };
                spent += frame.charge();
            }
        }
        assert_eq!(
            calls[1..],
            [(2, 0), (2, 0), (2, 0), (2, 0), (1, 0)],
            "(keys given, allocations) of each call after the first: {calls:?}"
        );
    })
    .expect("the run ends");
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    woken_of_a_reader_that_takes_each_frame();
    woken_of_a_reader_that_falls_behind_again();
    woken_of_a_reader_whose_frame_waits_for_credit();
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, now) = hub(&node, tasks).await;
        let mut reader = hub
            .reader(&[name("value")], Mode::Complete)
            .await
            .expect("opens");
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: vec![name("time"), name("value")],
        };
        let mut writer = hub.writer(config).await.expect("opens");
        let mut latest = hub
            .reader(&[name("value")], Mode::Latest)
            .await
            .expect("opens");
        for n in 0..WARM + COUNTED {
            let stamped = ALLOCATOR.count(|| writer.now()).1;
            let written = write(&mut writer, now + n);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                let counts = (stamped, written, read);
                assert_eq!(counts, (0, 0, 0), "frame {n} allocated");
            }
        }
        let now = now + WARM + COUNTED;
        for n in 0..WARM + COUNTED {
            let waited = wait(&mut reader);
            let written = write(&mut writer, now + n);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                assert_eq!((waited, written, read), (0, 0, 0), "frame {n} allocated");
            }
        }
        let now = now + WARM + COUNTED;
        read(&mut latest);
        for n in 0..WARM + COUNTED {
            let waited = wait(&mut latest);
            let written = write(&mut writer, now + n);
            let taken = read(&mut latest);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                assert_eq!((waited, written, taken, read), (0, 0, 0, 0), "latest {n}");
            }
        }
        let now = now + WARM + COUNTED;
        five_latest(&hub, &node, &mut writer, latest, now).await;
        replaced(&hub, &node, &mut writer, now + 3 * (WARM + 2)).await;
    })
    .expect("the run ends");
}
