//! A write and `next` make no heap allocation once the hub has taken a few frames:
//! for a complete reader when frames wait for it and when it waits for them, and for
//! a latest reader that the write wakes. The commit task is not counted. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::{Heap, Pool};
use env::tasks::Tasks;
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

/// A hub on a new ring of `node`, with `time` and `value` defined, and the node's
/// mesh time now once it has one.
async fn hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
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
        limits: home::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    let hub = Hub::new(hub::Config {
        home,
        interner,
        pool,
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
    loop {
        if let Some(now) = mesh.now().mesh {
            return (hub, now.latest.nanos());
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    }
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
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
    })
    .expect("the run ends");
}
