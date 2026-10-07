//! One write and one read, as a crate that depends on `home` makes them.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use block::{Heap, Pool};
use buffer::{Buffer, Layout};
use env::tasks::Tasks;
use home::{Config, Outcome, Shard, order, writer};
use types::authority::Authority;
use types::channel::{self, Slot, Slots};
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{Draft, Form, Label, Path, Range};
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

const STAMPS: channel::Key = channel::Key::from_u128(1);
const VALUES: channel::Key = channel::Key::from_u128(2);

/// A shard over a new ring of `node`, once the node has mesh time. It carries the
/// index of `STAMPS`, at slot 0.
async fn shard(node: sim::node::Node, tasks: Tasks, pool: &Rc<Pool>) -> Shard {
    let (clock, reader) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool: Rc::clone(pool),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        layout: Layout::new(1 << 18, 4087).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
    };
    let mut slots = Slots::new();
    let index = slots.assign(STAMPS);
    slots.assign(VALUES);
    let buffer = Buffer::open(config, &mut slots).await.expect("opens");
    while reader.now().mesh.is_none() {
        node.clock().sleep(Span::from_nanos(1)).await;
    }
    let mut shard = Shard::new(Config {
        shard: 0,
        buffer,
        clock: reader,
        limits: order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    shard.carry(index);
    shard
}

/// The key set of one index, `STAMPS`, with one `i64` channel, `VALUES`.
fn key_set() -> Arc<KeySet> {
    let mut interner = Interner::new();
    interner.slots().assign(STAMPS);
    interner.slots().assign(VALUES);
    interner.intern(&[Group {
        index: STAMPS,
        data: &[(VALUES, Type::Scalar(Scalar::I64))],
    }])
}

/// A raw frame of `set` with one sample: `stamp` and `value`.
fn frame(pool: &Pool, set: &KeySet, stamp: i64, value: i64) -> Draft {
    let mut draft =
        Draft::new(pool, set, Form::Raw, &[(0, 8), (1, 8)]).expect("a frame");
    for (entry, sample) in [(0, stamp), (1, value)] {
        let series = draft.series_mut(entry).expect("the series is present");
        series.copy_from_slice(&sample.to_le_bytes());
    }
    draft.set_count(0, 1);
    draft
}

async fn writes_and_reads(node: sim::node::Node, tasks: Tasks) {
    let config = block::Config { budget: 1 << 21 };
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let mut shard = shard(node, tasks, &pool).await;
    let set = key_set();
    let index = Slot::new(0);
    let reader = shard.open_complete(index, 1 << 20).expect("synced");
    let writer = shard
        .open_writer(writer::Writer {
            subject: "a".parse().expect("a valid name"),
            authority: Authority(1),
            lease: None,
            set: Arc::clone(&set),
        })
        .expect("synced");
    let range = Range { seq: 0, count: 1 };

    let written =
        shard.write(writer, Label::Path(Path::Live), frame(&pool, &set, 10, 7));
    assert_eq!(written, Ok(&[Outcome::Applied { slot: index, range }][..]));
    shard.committed().await.expect("the commit ends");
    let mut woken = Vec::new();
    shard.woken(&mut woken);
    assert_eq!(woken, [reader.into()]);
    let taken = shard.take(reader.into()).expect("a frame waits");
    assert_eq!(taken.range(0), Some(range));
    shard.close_reader(reader.into());
    shard.close_writer(writer);
}

#[test]
fn writes_and_reads_a_frame_from_outside_the_crate() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: None,
    };
    let handle = node
        .shards()
        .start(config, move |tasks| writes_and_reads(node, tasks))
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}
