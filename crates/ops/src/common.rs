//! Fixtures that the tests of `plan`, `apply`, and `Node` share.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use connector::cancel;
use connector::kind::{self, Channels, Context};
use document::diagnostic::Diagnostic;
use document::{Document, Source};
use env::files::{self, Operation};
use env::tasks::Tasks;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use mesh::region::Founding;
use mesh::status::Status;
use mesh::{Member, Mesh};
use sim::Sim;
use spec::definition::Definition;
use transport::{Port, Transport};
use types::channel::Key;
use types::ed25519::PrivateKey;
use types::name::{Name, Prefix};
use types::node::{self, SealKey};
use types::time::Span;

use crate::front_end::{File, FrontEnd};

pub(crate) const PLANT: &str =
    include_str!("../../acceptance/tests/it/fixtures/plant.hcl");
pub(crate) const SITE: &str =
    include_str!("../../acceptance/tests/it/fixtures/site.hcl");

/// A kind whose channels are the labels of its `read` blocks, which it writes. It takes
/// each attribute, so it stands in for each kind of the fixtures.
pub(crate) struct Reader;

impl kind::Kind for Reader {
    type Config = Vec<Name>;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let reads = config
            .blocks
            .iter()
            .filter(|block| &*block.keyword == "read");
        Ok(reads
            .map(|block| block.labels[0].text.parse().expect("a name"))
            .collect())
    }

    fn check(&self, writes: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reads: Vec::new(),
            writes: writes.clone(),
            counts: Vec::new(),
        })
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

pub(crate) fn hcl(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>> {
    config_hcl::read(source, text)
        .map_err(|errors| errors.iter().map(Diagnostic::from).collect())
}

pub(crate) fn front_ends() -> BTreeMap<&'static str, FrontEnd> {
    BTreeMap::from([("hcl", FrontEnd { read: hcl })])
}

pub(crate) fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

pub(crate) fn files(files: &[(&str, &str)]) -> Vec<File> {
    files
        .iter()
        .map(|(path, text)| File {
            path: PathBuf::from(path),
            text: (*text).to_owned(),
        })
        .collect()
}

/// `site.hcl` with a placement that homes its index on `edge`.
pub(crate) fn placed_site() -> String {
    format!("{SITE}placement \"p\" {{\n  select = \"site.*\"\n  home = \"edge\"\n}}\n")
}

/// The member number of this node.
const OWN: u8 = 1;
pub(crate) const NODE: node::Key = key(OWN);
pub(crate) const PRIVATE_KEY: PrivateKey = private_key(OWN);
pub(crate) const ADMIN: PrivateKey = PrivateKey([7; 32]);
pub(crate) const PORT: u16 = 7000;

/// The key of the member `n`.
const fn key(n: u8) -> node::Key {
    node::Key::from_u128(n as u128)
}

/// The private key of the member `n`.
const fn private_key(n: u8) -> PrivateKey {
    PrivateKey([n; 32])
}

/// The record of the member `n`, named `name`.
pub(crate) fn create_member(n: u8, name: &str) -> Member {
    let private_key = private_key(n);
    let card = Card {
        name: self::name(name),
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
pub(crate) async fn open(
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
        entropy: node.entropy(),
        tasks: tasks.clone(),
        transport: Rc::new(transport),
        pool,
        store: Rc::new(store),
    };
    Mesh::open(config).await.expect("a mesh")
}

/// Runs `body` with the one node of a run and the mesh of [`open`] on it, whose one
/// member is `edge`, with the definitions of a first start.
pub(crate) fn solo<F: Future<Output = ()> + 'static>(
    body: impl FnOnce(sim::node::Node, Mesh) -> F + Send + 'static,
) {
    founded(&["edge"], spec::founding::create(ADMIN.public()), body);
}

/// Runs `body` with the one node of a run and the mesh of [`open`] on it, with the
/// members `names` and the founding `definitions`.
pub(crate) fn founded<F: Future<Output = ()> + 'static>(
    names: &'static [&'static str],
    definitions: BTreeMap<Name, Definition>,
    body: impl FnOnce(sim::node::Node, Mesh) -> F + Send + 'static,
) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, names, definitions).await;
        body(node, mesh).await;
    });
    assert_eq!(ran, Ok(()));
}

/// Makes each sync of the log of the mesh of [`open`] on `node` fail, and gives the
/// stop of its group at its next write of the log.
pub(crate) fn fail_sync(node: &sim::node::Node) -> mesh::Stopped {
    let path = Path::new("log").join("log-0");
    node.fail_file(&path, Operation::Sync);
    let cause = files::Error::Io {
        path,
        operation: Operation::Sync,
        code: 5,
    };
    mesh::Stopped::Write(mesh::log::Error::Files(cause))
}

/// Gives `Key::from_u128(n)` for each `n` from `from` up.
pub(crate) fn keys(from: u128) -> impl FnMut() -> Key {
    let mut next = from;
    move || {
        next += 1;
        Key::from_u128(next)
    }
}
