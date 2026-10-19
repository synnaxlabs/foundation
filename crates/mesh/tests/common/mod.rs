//! The config of a mesh of one node for the tests that count the heap.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::rc::Rc;

use env::tasks::Tasks;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use mesh::region::Founding;
use mesh::status::Status;
use mesh::{Config, Member};
use sim::node::Node;
use transport::{Port, Transport};
use types::ed25519::PrivateKey;
use types::name::Prefix;
use types::node::{self, SealKey};
use types::time::Span;

mod steps;

pub(crate) use steps::simulation;

pub(crate) const KEY: node::Key = node::Key::from_u128(1);
pub(crate) const PRIVATE_KEY: PrivateKey = PrivateKey([1; 32]);

/// The config of the region `plant`, whose one member and one voter is the node `KEY`.
pub(crate) async fn create_config(node: &Node, tasks: &Tasks) -> Config {
    let budget = block::Config::new(1 << 20).expect("the budget fits");
    let memory = block::Heap::new(budget.reservation());
    let pool = Rc::new(block::Pool::new(budget, memory));
    let at = SocketAddr::new(node.addresses()[0], 7000);
    let port = Port::bind(&node.net(), at).expect("a port");
    let mut parts = port.split(NonZeroUsize::MIN);
    let transport = transport::Config {
        private_key: PRIVATE_KEY,
        message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
    };
    let transport =
        Transport::new(transport, parts.pop().expect("one part")).expect("a transport");
    let store = blob::Store::open(blob::Config {
        files: node.files(),
        dir: "blob".into(),
        pool: Rc::clone(&pool),
        floor_bytes: 0,
    })
    .await
    .expect("the store opens");
    let card = Card {
        name: "plant.node1".parse().expect("a name"),
        public_key: PRIVATE_KEY.public(),
        seal_key: SealKey::new([9; 32]).expect("a seal key"),
        addresses: Addresses::new(Vec::new()).expect("no address"),
        version: 1,
    };
    let member = Member {
        card: card::Signed::sign(KEY, card, &PRIVATE_KEY),
        admission: [0; 64],
        ephemeral: None,
        status: Status::new([].into()).expect("an empty status"),
    };
    Config {
        key: KEY,
        private_key: PRIVATE_KEY,
        founding: Founding {
            prefix: "plant".parse::<Prefix>().expect("a prefix"),
            members: vec![member],
            voters: [KEY].into(),
            definitions: BTreeMap::new(),
            homes: BTreeMap::new(),
        },
        files: node.files(),
        dir: PathBuf::new(),
        clock: node.clock(),
        time: clock::Clock::new(node.clock()).1,
        entropy: node.entropy(),
        tasks: tasks.clone(),
        transport: Rc::new(transport),
        pool,
        store: Rc::new(store),
    }
}
