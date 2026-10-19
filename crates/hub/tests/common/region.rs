//! The region of the hub tests, at the root prefix, whose one voter is the node of a
//! hub under test.

use std::collections::BTreeMap;
use std::rc::Rc;

use env::tasks::Tasks;
use mesh::Member;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use spec::definition::Definition;
use transport::{Address, Transport};
use types::ed25519::PrivateKey;
use types::name::{Name, Prefix};
use types::node::SealKey;

use crate::net::{HOME, NODE, PEER, own_pool, public_key};

/// The other member of the region, which is not a voter.
pub(crate) const OTHER: types::node::Key = types::node::Key::from_u128(2);

/// The region at [`NODE`] over `transport`, whose one voter is [`NODE`], whose
/// other member is [`OTHER`] at `other`, and whose founding spec holds `definitions`.
/// At the root prefix, it governs each name.
pub(crate) async fn open(
    node: &sim::node::Node,
    tasks: &Tasks,
    transport: Rc<Transport>,
    other: Vec<Address>,
    definitions: BTreeMap<Name, Definition>,
) -> hub::Region {
    let pool = own_pool();
    let store = blob::Store::open(blob::Config {
        files: node.files(),
        dir: "blob".into(),
        pool: Rc::clone(&pool),
        floor_bytes: 0,
    })
    .await
    .expect("the store opens");
    let config = mesh::Config {
        key: NODE,
        private_key: HOME,
        founding: mesh::region::Founding {
            prefix: Prefix::ROOT,
            members: vec![member(NODE, &HOME, Vec::new()), member(OTHER, &PEER, other)],
            voters: [NODE].into(),
            definitions,
            homes: BTreeMap::new(),
        },
        files: node.files(),
        dir: "mesh".into(),
        clock: node.clock(),
        time: clock::Clock::new(node.clock()).1,
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool,
        transport: Rc::clone(&transport),
        store: Rc::new(store),
    };
    let mesh = mesh::Mesh::open(config).await.expect("the mesh opens");
    hub::Region { mesh, transport }
}

/// The member `key` of the region at `addresses`.
fn member(
    key: types::node::Key,
    private_key: &PrivateKey,
    addresses: Vec<Address>,
) -> Member {
    let card = Card {
        name: format!("plant.node{key}").parse().expect("a name"),
        public_key: public_key(private_key),
        seal_key: SealKey::new([9; 32]).expect("a seal key"),
        addresses: Addresses::new(addresses).expect("addresses"),
        version: 1,
    };
    Member {
        card: card::Signed::sign(key, card, private_key),
        admission: [0; 64],
        ephemeral: None,
        status: mesh::status::Status::new(BTreeMap::new()).expect("a status"),
    }
}
