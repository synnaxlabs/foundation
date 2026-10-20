//! The hub of a `hub` bench of a serve: a new shard whose ring holds the run, and its
//! channels.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::rc::Rc;

use block::{Heap, Pool};
use env::tasks::Tasks;
use hub::Hub;
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::frame::key_set::Interner;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

use crate::common::name;

/// A ring that holds each frame of a run of 2000 rounds of one line, as nothing frees
/// a ring until #160: 256 times the ring of `home::testing::shard`. The sim disk keeps
/// only the bytes written.
const AREA: u64 = 1 << 30;
/// A pool of 4 times the budget of `home::testing::shard`. A size class keeps its
/// range while one of its blocks is in use, so the lines of a bench before its widest
/// can hold most of 8 MiB, which then has no room for a frame of the widest in some
/// orders of the tasks.
const BUDGET: u64 = 1 << 25;

/// A hub on a new shard, and the node's mesh time now. Each of `indexes` is the key of
/// an index and the keys of its data channels of `i64`, each named by
/// [`channel_name`].
pub(crate) async fn create_hub(
    node: &sim::node::Node,
    tasks: Tasks,
    indexes: impl IntoIterator<Item = (u128, RangeInclusive<u128>)>,
) -> (Hub, i64) {
    let (home, interner, now, time) = create_shard(node, tasks.clone()).await;
    let hub = Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: crate::net::NODE,
        public_key: crate::net::public_key(&crate::net::HOME),
        time,
        entropy: node.entropy(),
        region: None,
    });
    let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
    let mut definitions = Vec::new();
    for (index, data) in indexes {
        let key = channel::Key::from_u128(index);
        let kind = Kind::Index {
            error: None,
            control: None,
        };
        definitions.push((index, Channel { key, kind }));
        for data in data {
            let data_key = channel::Key::from_u128(data);
            let kind =
                Kind::Data(Data::new(key, None, i64.clone(), None).expect("no unit"));
            definitions.push((
                data,
                Channel {
                    key: data_key,
                    kind,
                },
            ));
        }
    }
    let definitions: Vec<_> = definitions
        .into_iter()
        .map(|(key, channel)| (channel_name(key), Definition::Channel(channel)))
        .collect();
    hub.set_definitions(
        definitions
            .iter()
            .map(|(name, definition)| (name, definition)),
    );
    (hub, now)
}

/// The name of the channel `key`.
pub(crate) fn channel_name(key: u128) -> types::name::Name {
    name(&format!("c{key}"))
}

/// `home::testing::shard` on a ring of `AREA` and a pool of `BUDGET`, its interner,
/// the mesh time once the clock has one, and the reader of that time. #160 removes it.
async fn create_shard(
    node: &sim::node::Node,
    tasks: Tasks,
) -> (home::Shard, Interner, i64, clock::Reader) {
    let config = block::Config::new(BUDGET).expect("the budget fits");
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
        tasks,
        entropy: node.entropy(),
        layout: buffer::Layout::new(AREA, 1 << 16).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
        trimmed: false,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = home::Shard::open(home::Config {
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
            return (
                shard,
                interner,
                now.earliest.nanos().midpoint(now.latest.nanos()),
                mesh,
            );
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    }
}
