//! A hub on one shard of a sim node, with `time` and `value` on it, and the writes to
//! it, for the tests and benches that count or time the hub.

use std::collections::BTreeMap;

use env::tasks::Tasks;
use hub::Hub;
use hub::home::Outcome;
use hub::reader;
use hub::writer::Writer;
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::frame::{Draft, Form, Label, Path};
use types::name::{Name, Selector};
use types::sample::{Scalar, Type};
use types::time::Span;

use crate::node::env;

/// Past the commit of a write.
pub(crate) const SETTLE: Span = Span::from_nanos(20_000_000);

/// A hub on a new shard, with `time` and `value` defined, and the node's mesh time now.
pub(crate) async fn hub(node: &sim::node::Node, tasks: Tasks) -> (Hub, i64) {
    let (hub, now) = hub::testing::open(env(node, tasks)).await;
    hub.set_definitions(&definitions(&[]));
    (hub, now.nanos())
}

/// A hub on a new shard of node 1 in `region`, with mesh time, which takes its
/// definitions from the mesh of `region`.
pub(crate) async fn hub_in(
    node: &sim::node::Node,
    tasks: Tasks,
    region: hub::Region,
) -> Hub {
    hub_on(node, tasks, Some(region)).await
}

/// A hub on a new shard of node 1 with no region, with mesh time, and with `time`,
/// `value`, and each of `more`, an `i64` channel on `time`, defined.
pub(crate) async fn hub_alone(
    node: &sim::node::Node,
    tasks: Tasks,
    more: &[Name],
) -> Hub {
    let hub = hub_on(node, tasks, None).await;
    hub.set_definitions(&definitions(more));
    hub
}

/// A hub on a new shard of node 1 in `region`, or with no region, with mesh time.
async fn hub_on(
    node: &sim::node::Node,
    tasks: Tasks,
    region: Option<hub::Region>,
) -> Hub {
    let env = env(node, tasks);
    let (tasks, entropy) = (env.tasks.clone(), env.entropy.clone());
    let (home, interner, _, time) = home::testing::shard(env).await;
    Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        public_key: types::ed25519::PrivateKey([1; 32]).public(),
        time,
        entropy,
        region,
    })
}

/// `time` with key 1, `value` with key 2, and each of `more`, from key 100 on, each
/// an `i64` channel on `time`.
pub(crate) fn definitions(more: &[Name]) -> BTreeMap<Name, Definition> {
    let time = Channel {
        key: channel::Key::from_u128(1),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    };
    let index = time.key;
    let data = |key| {
        let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
        Definition::Channel(Channel {
            key: channel::Key::from_u128(key),
            kind: Kind::Data(Data::new(index, None, i64, None).expect("no unit")),
        })
    };
    let mut definitions = BTreeMap::from([
        (name("time"), Definition::Channel(time)),
        (name("value"), data(2)),
    ]);
    definitions.extend(
        (100..)
            .zip(more)
            .map(|(key, name)| (name.clone(), data(key))),
    );
    definitions
}

pub(crate) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// An unnamed reader on the channels named `channels`.
pub(crate) fn unnamed(channels: &[&str], mode: reader::Mode) -> reader::Config {
    reader::Config {
        select: Selector::new(channels.iter().copied()).expect("a selector"),
        mode,
        subject: name("reader"),
        name: None,
        hold: Span::ZERO,
    }
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
