//! A write and `next` make no heap allocation once the hub has taken a few frames: for
//! a complete reader when frames wait for it and when it waits for them, and for latest
//! readers that the write wakes, also two writes in one commit and readers replaced.
//! The commit task is not counted. The home's `woken` after a commit, which that task
//! calls, is counted on its own: it makes none once its keys are sized, while a
//! complete reader takes each frame. This binary has no test harness: the count covers
//! each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use block::{Heap, Pool};
use env::tasks::Tasks;
use hub::reader::{Mode, Reader};
use hub::writer::{self, Writer};
use hub::{Channel, Hub};
use types::authority::Authority;
use types::channel;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const COMMIT: Span = Span::from_nanos(10_000_000);
/// Past the commit of a write.
const SETTLE: Span = Span::from_nanos(20_000_000);
/// Frames that warm the hub up: its vectors reach their size.
const WARM: i64 = 4;
/// Frames whose write and read are counted. More than the streak of `next`, so a
/// count covers its yield.
const COUNTED: i64 = 300;

fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// Writes one sample to `time` and to `value`, with the allocations the write made.
fn write(writer: &mut Writer, stamp: i64) -> u64 {
    let set = writer.set();
    let entries = set.entries();
    let entry = |key| {
        let key = channel::Key::from_u128(key);
        entries
            .iter()
            .position(|entry| entry.key == key)
            .expect("the key set holds the channel")
    };
    let (time, value) = (entry(1), entry(2));
    let group = entries[time].group;
    let (written, allocations) = ALLOCATOR.count(|| {
        let mut draft = writer
            .draft(Form::Raw, &[(time, 8), (value, 8)])
            .expect("a frame");
        for entry in [time, value] {
            let bytes = draft.series_mut(entry).expect("the series is present");
            bytes.copy_from_slice(&stamp.to_le_bytes());
        }
        draft.set_count(group, 1);
        writer.write(Label::Path(Path::Live), draft).map(<[_]>::len)
    });
    assert_eq!(written, Ok(1), "the home applies the frame");
    allocations
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

/// A home shard on a new ring of `node`, its interner, and the node's mesh time now
/// once it has one.
async fn shard(node: &sim::node::Node, tasks: Tasks) -> (home::Shard, Interner, i64) {
    let config = block::Config { budget: 1 << 23 };
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool: Rc::clone(&pool),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        // A frame takes 4 KiB of the ring, and a full ring loses each later live
        // frame (#160), so the test writes fewer than 1024.
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: COMMIT,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = home::Shard::new(home::Config {
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
            return (shard, interner, now.latest.nanos());
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    }
}

/// A hub on [`shard`], with `time` and `value` defined, and the node's mesh time now.
async fn hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
    let (home, interner, now) = shard(node, tasks.clone()).await;
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
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
    (hub, now)
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

/// The home's `woken` with one complete reader that takes each frame: each call
/// after the first names the reader and allocates nothing.
fn woken_of_a_reader_that_takes_each_frame() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (mut shard, mut interner, now) = shard(&node, tasks).await;
        let (time, value) = (channel::Key::from_u128(1), channel::Key::from_u128(2));
        let slot = interner.slots().assign(time);
        shard.carry(slot);
        let data = vec![(value, Type::Scalar(Scalar::I64))];
        let set = interner.intern(&[Group {
            index: time,
            data: &data,
        }]);
        let writer = shard
            .open_writer(home::writer::Writer {
                subject: name("a"),
                authority: Authority(1),
                lease: None,
                set: Arc::clone(&set),
            })
            .expect("opens");
        let reader = shard.open_complete(slot, u64::MAX).into();
        let entries = set.entries();
        let entry = |key| {
            entries
                .iter()
                .position(|entry| entry.key == key)
                .expect("the key set holds the channel")
        };
        let (time, value) = (entry(time), entry(value));
        let mut keys = Vec::new();
        let mut calls = Vec::new();
        for n in 0..20 {
            let mut draft =
                Draft::new(shard.pool(), &set, Form::Raw, &[(time, 8), (value, 8)])
                    .expect("a frame");
            for entry in [time, value] {
                let bytes = draft.series_mut(entry).expect("the series is present");
                bytes.copy_from_slice(&(now + n).to_le_bytes());
            }
            draft.set_count(entries[time].group, 1);
            let written = shard.write(writer, Label::Path(Path::Live), draft);
            assert!(written.is_ok(), "the home applies frame {n}");
            shard.committed().await.expect("commits");
            let ((), allocations) = ALLOCATOR.count(|| shard.woken(&mut keys));
            calls.push((keys.len(), allocations));
            assert!(
                shard.take(reader).is_some(),
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

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    woken_of_a_reader_that_takes_each_frame();
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
            let written = write(&mut writer, now + n);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                assert_eq!((written, read), (0, 0), "frame {n} allocated");
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
