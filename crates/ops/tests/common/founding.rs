//! The mesh of a root region from its founding, for the unit tests of `ops` and its
//! test binaries. Each mounts this file with `#[path]`.

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
use mesh::{Member, Mesh};
use spec::definition::Definition;
use transport::{Port, Transport};
use types::ed25519::PrivateKey;
use types::name::{Name, Prefix};
use types::node::{self, SealKey};
use types::time::Span;

/// The member number of this node.
const OWN: u8 = 1;
pub(crate) const NODE: node::Key = key(OWN);
pub(crate) const ADMIN: PrivateKey = PrivateKey([7; 32]);
const PRIVATE_KEY: PrivateKey = private_key(OWN);
const PORT: u16 = 7000;

/// The key of the member `n`.
const fn key(n: u8) -> node::Key {
    node::Key::from_u128(n as u128)
}

/// The private key of the member `n`.
const fn private_key(n: u8) -> PrivateKey {
    PrivateKey([n; 32])
}

/// The record of the member `n`, named `name`.
fn create_member(n: u8, name: &str) -> Member {
    let private_key = private_key(n);
    let card = Card {
        name: name.parse().expect("a name"),
        public_key: private_key.public(),
        seal_key: SealKey::new([9; 32]).expect("a seal key"),
        addresses: Addresses::new(Vec::new()).expect("no addresses"),
        version: 1,
    };
    Member {
        card: card::Signed::sign(key(n), card, &private_key),
        admission: [0; 64],
        ephemeral: None,
        status: Status::new([].into()).expect("a status"),
    }
}

/// The mesh of a root region with the founding `definitions`, whose members are
/// `names` at the keys from `NODE` up. The first is this node and the one voter.
pub(crate) async fn create_mesh(
    node: &sim::node::Node,
    tasks: &Tasks,
    names: &[&str],
    definitions: BTreeMap<Name, Definition>,
) -> Mesh {
    let budget = block::Config::new(1 << 20).expect("the budget fits");
    let memory = block::Heap::new(budget.reservation());
    let pool = Rc::new(block::Pool::new(budget, memory));
    let at = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), at)
        .expect("a port")
        .split(NonZeroUsize::MIN);
    let config = transport::Config {
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
    let part = parts.pop().expect("a part");
    let transport = Transport::new(config, part).expect("a transport");
    let store = blob::Store::open(blob::Config {
        files: node.files(),
        dir: "blob".into(),
        pool: Rc::clone(&pool),
        floor_bytes: 0,
    })
    .await
    .expect("a store");
    let config = mesh::Config {
        key: NODE,
        private_key: PRIVATE_KEY,
        founding: Founding {
            prefix: Prefix::ROOT,
            voters: [NODE].into(),
            members: (OWN..)
                .zip(names)
                .map(|(n, name)| create_member(n, name))
                .collect(),
            definitions,
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
    };
    Mesh::open(config).await.expect("a mesh")
}
