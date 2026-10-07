//! A hub on one shard of a sim node, with `time` and `value` on it, and the writes to
//! it, for the tests and benches that count or time the hub.

use std::path::PathBuf;
use std::rc::Rc;

use block::{Heap, Pool};
use env::tasks::Tasks;
use hub::home::Outcome;
use hub::writer::Writer;
use hub::{Channel, Hub};
use types::channel;
use types::frame::key_set::Interner;
use types::frame::{Draft, Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

const COMMIT: Span = Span::from_nanos(10_000_000);
/// Past the commit of a write.
pub(crate) const SETTLE: Span = Span::from_nanos(20_000_000);

pub(crate) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// A home shard on a new ring of `node`, its interner, and the node's mesh time now
/// once it has one.
pub(crate) async fn shard(
    node: &sim::node::Node,
    tasks: Tasks,
) -> (home::Shard, Interner, i64) {
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
        // A commit takes whole 4 KiB blocks (one for a frame, three for 64), and
        // nothing frees the ring until #160, so a run fills it at 1023 one-frame
        // commits.
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
pub(crate) async fn hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
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

/// A frame of one sample at `stamp` on `time` and `value`.
pub(crate) fn draft(writer: &Writer, stamp: i64) -> Draft {
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
    let mut series = [(time, 8), (value, 8)];
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for entry in [time, value] {
        let bytes = draft.series_mut(entry).expect("the series is present");
        bytes.copy_from_slice(&stamp.to_le_bytes());
    }
    draft.set_count(entries[time].group, 1);
    draft
}

/// Writes `draft` as a live frame.
///
/// # Panics
///
/// When the home does not apply it.
pub(crate) fn write(writer: &mut Writer, draft: Draft) {
    let outcomes = writer
        .write(Label::Path(Path::Live), draft)
        .expect("the home takes it");
    assert!(
        matches!(outcomes, [Outcome::Applied { .. }]),
        "the home applies the frame: {outcomes:?}"
    );
}
