//! A hub on one shard of a sim node, with `time` and `value` on it, and the writes to
//! it, for the tests and benches that count or time the hub.

use env::tasks::Tasks;
use hub::home::Outcome;
use hub::writer::Writer;
use hub::{Channel, Hub};
use types::channel;
use types::frame::{Draft, Form, Label, Path};
use types::sample::{Scalar, Type};
use types::time::Span;

use crate::shard::{name, shard};

/// Past the commit of a write.
pub(crate) const SETTLE: Span = Span::from_nanos(20_000_000);

/// A hub on a new shard, with `time` and `value` defined, and the node's mesh time now.
pub(crate) async fn hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
    let (home, interner, now, time) = shard(node, tasks.clone()).await;
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time,
        clock: node.clock(),
        entropy: node.entropy(),
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
