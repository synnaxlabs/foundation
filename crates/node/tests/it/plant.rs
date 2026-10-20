//! A node process whose spec holds the plant channels, and the node and admin of a
//! node process. The tests of `sdk` and the `sdk` bench use them.

use std::rc::Rc;

use env::tasks::Tasks;
use sdk::write::{Config, Writer};
use sdk::{Client, Node, Subject};

use crate::rig::Rig;

/// The node of `rig`, with an unspecified IP as loopback, and its admin, once the node
/// wrote both.
pub(crate) fn admin(rig: &Rig) -> (Node, Subject) {
    let dir = rig.dir.join("foundation-data");
    rig.wait(
        "the node writes its address and admin",
        || match Node::local(&dir).map_err(|e| e.to_string())? {
            (node, Some(subject)) => Ok((node, subject)),
            (_, None) => Err("no admin".to_owned()),
        },
    )
}

/// The channels of the tests of writers and readers: the index `plant.time`, and
/// `plant.a` and `plant.b` of `i64` on it, and the index `cell.time` and `cell.c` of
/// `i64` on it, homed at the node `edge`.
const CHANNELS: &str = concat!(
    "channel \"plant.time\" { kind = \"index\" }\n",
    "channel \"plant.a\" {\n  data_type = \"i64\"\n  index = \"plant.time\"\n}\n",
    "channel \"plant.b\" {\n  data_type = \"i64\"\n  index = \"plant.time\"\n}\n",
    "channel \"cell.time\" { kind = \"index\" }\n",
    "channel \"cell.c\" {\n  data_type = \"i64\"\n  index = \"cell.time\"\n}\n",
    "placement \"plant.home\" {\n  select = \"plant.*\"\n  home = \"edge\"\n}\n",
    "placement \"cell.home\" {\n  select = \"cell.*\"\n  home = \"edge\"\n}\n",
);

/// A rig whose node runs with [`CHANNELS`] applied, its node, and its admin. Waits
/// until the hub of the node knows the channels, which it learns after the apply.
pub(crate) fn planted() -> (Rig, Node, Subject) {
    planted_on(Rig::new())
}

/// [`planted`] on `rig`, which has no node yet.
pub(crate) fn planted_on(mut rig: Rig) -> (Rig, Node, Subject) {
    rig.config(CHANNELS);
    rig.start();
    rig.apply();
    let (node, subject) = admin(&rig);
    let client = Client::connect(&node, subject.clone()).unwrap();
    rig.wait("the hub knows the channels", || {
        let channels = vec!["plant.a".parse().unwrap(), "plant.b".parse().unwrap()];
        let writer = client.open_writer(Config::new(channels));
        writer.and_then(Writer::close).map_err(|e| e.to_string())?;
        let writer = client.open_writer(Config::new(vec!["cell.c".parse().unwrap()]));
        writer.and_then(Writer::close).map_err(|e| e.to_string())
    });
    assert_eq!(client.close(), Ok(()));
    (rig, node, subject)
}

/// A session with `node` as `subject`, as the client of `sdk` makes it, with a pool of
/// `frames` bytes for its drafts: its other pools have the budgets of `POOL` and
/// `READS` of `sdk/rust/src/lib.rs`, which a change there must follow. `FRAMES` there
/// is 64 KiB.
pub(crate) async fn session(
    node: Node,
    subject: Subject,
    tasks: Tasks,
    frames: u64,
) -> program::Session {
    let budget = |bytes| block::Config::new(bytes).unwrap();
    let clock = os::clock();
    let entropy = os::entropy();
    let pool = Rc::new(block::Pool::heap(budget(1 << 21)));
    let transport = transport::Client::new(transport::client::Config {
        net: os::net(),
        clock: clock.clone(),
        entropy: entropy.clone(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
    })
    .unwrap();
    let frames = Rc::new(block::Pool::heap(budget(frames)));
    let config = program::Config {
        node: node.public_key,
        addresses: node
            .addresses
            .into_iter()
            .map(transport::Address::Udp)
            .collect(),
        subject: subject.name,
        key: subject.private_key,
        clock,
        entropy,
        tasks,
        pool,
        frames,
        reads: Rc::new(block::Pool::heap(budget(8 << 20))),
    };
    program::Session::connect(&transport, config).await.unwrap()
}
