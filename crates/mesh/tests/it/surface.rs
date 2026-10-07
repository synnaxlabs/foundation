//! What a crate outside `mesh` opens and reads of a region, as `node` and `hub` do.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use env::tasks::Tasks;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use mesh::status::Status;
use mesh::{Config, Error, Member, Mesh, Stopped, Watch, change, claim, log, region};
use raft::{Position, Term};
use sim::Sim;
use transport::stream::Incoming;
use transport::{Address, Class, Code, Peer, Port, Transport};
use types::channel;
use types::name::Prefix;
use types::node::{self, PrivateKey, PublicKey, SealKey};
use types::time::Span;
use wire::Protocol;

const KEY: node::Key = node::Key::from_u128(1);
const OTHER: node::Key = node::Key::from_u128(2);
const INDEX: channel::Key = channel::Key::from_u128(7);
/// The port of each transport.
const PORT: u16 = 7000;

type Home = Result<Option<node::Key>, Stopped>;

fn assert_gives_a_home<'a, F: Future<Output = Home>>(_: fn(&'a mut Watch) -> F) {}

fn assert_serves<'a, F: Future<Output = Result<(), Error>>>(
    _: fn(&'a Mesh, PublicKey, Incoming) -> F,
) {
}

fn assert_error<E: std::error::Error>(_: &E) {}

fn private_key() -> PrivateKey {
    PrivateKey([1; 32])
}

fn public_key() -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&private_key().0).unwrap();
    PublicKey::new(pair.public_key().as_ref().try_into().unwrap()).unwrap()
}

/// The record of the node `KEY`, with a card that the node signed.
fn create_member() -> Member {
    let card = Card {
        name: "plant.node1".parse().unwrap(),
        public_key: public_key(),
        seal_key: SealKey::new([9; 32]).unwrap(),
        addresses: Addresses::new(Vec::new()).unwrap(),
        version: 1,
    };
    Member {
        card: card::Signed::sign(KEY, card, &private_key()),
        admission: [0; 64],
        ephemeral: None,
        status: Status::new([].into()).unwrap(),
    }
}

fn create_pool() -> Rc<block::Pool> {
    let budget = block::Config { budget: 1 << 20 };
    let memory = block::Heap::new(budget.reservation());
    Rc::new(block::Pool::new(budget, memory))
}

/// A transport of `node` at `PORT`, which proves the public key of `private_key`.
fn create_transport(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<block::Pool>,
    private_key: PrivateKey,
) -> Transport {
    let at = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), at)
        .unwrap()
        .split(NonZeroUsize::MIN);
    let config = transport::Config {
        private_key,
        message_bytes_max: NonZeroUsize::new(1 << 16).unwrap(),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).unwrap(),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: Rc::clone(pool),
    };
    Transport::new(config, parts.pop().unwrap()).unwrap()
}

/// The config of the region `plant`, whose one member and one voter is the node `KEY`.
fn create_config(node: &sim::node::Node, tasks: &Tasks) -> Config {
    create_config_on(node, tasks, private_key())
}

/// That config, on a transport that proves the public key of `transport_key`.
fn create_config_on(
    node: &sim::node::Node,
    tasks: &Tasks,
    transport_key: PrivateKey,
) -> Config {
    let pool = create_pool();
    let transport = create_transport(node, tasks, &pool, transport_key);
    Config {
        key: KEY,
        private_key: private_key(),
        region: "plant".parse::<Prefix>().unwrap(),
        members: vec![create_member()],
        voters: [KEY].into(),
        files: node.files(),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        transport: Rc::new(transport),
        pool,
    }
}

/// Runs `body` on the one node of a run.
fn solo<F: Future<Output = ()> + 'static>(
    body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, body).unwrap();
}

#[test]
fn a_node_opens_its_region_and_reads_its_member_and_a_home() {
    solo(|node, tasks| async move {
        let mesh = Mesh::open(create_config(&node, &tasks)).await.unwrap();
        assert_eq!(mesh.member(KEY), Some(create_member()));
        assert_eq!(mesh.member(OTHER), None);
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        drop(mesh);
        assert_eq!(watch.next().await, Err(Stopped::Dropped));
    });
}

// A known gap until #1587: `open` must refuse this transport.
#[test]
fn open_takes_a_transport_that_proves_another_key() {
    solo(|node, tasks| async move {
        let config = create_config_on(&node, &tasks, PrivateKey([3; 32]));
        drop(Mesh::open(config).await.unwrap());
    });
}

#[test]
fn a_region_with_two_records_of_one_node_does_not_open() {
    solo(|node, tasks| async move {
        let mut config = create_config(&node, &tasks);
        config.members.push(create_member());
        let unfit = region::Unfit::Duplicate { key: KEY };
        assert_eq!(Mesh::open(config).await.err(), Some(Error::Member(unfit)));
    });
}

#[test]
fn the_debug_of_a_config_does_not_show_the_private_key() {
    solo(|node, tasks| async move {
        let debug = format!("{:?}", create_config(&node, &tasks));
        assert!(debug.contains("private_key: PrivateKey(..)"), "{debug}");
    });
}

// The peer is not a member. `serve` refuses the bytes before it reads who sent them.
#[test]
fn serve_refuses_a_message_that_is_not_valid_and_stops_its_stream() {
    let mut sim = Sim::new(sim::Config::default());
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(nodes[0].addresses()[0], PORT));
    let served = Arc::new(Mutex::new(None));
    let sent = Arc::new(Mutex::new(None));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let (node, result) = (nodes[0].clone(), Arc::clone(&served));
    let main = move |tasks: Tasks| async move {
        let config = create_config(&node, &tasks);
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let session = transport.accept().await.unwrap();
        let Peer::Node(peer) = session.peer() else {
            panic!("a peer with no node key opened a session");
        };
        let mut incoming = session.accept().await.unwrap();
        let header = incoming.receiver.recv().await.unwrap().unwrap();
        let protocol = wire::header::decode(&header).unwrap();
        assert_eq!(protocol, (Protocol::Mesh, &[][..]));
        drop(header);
        *result.lock().unwrap() = Some(mesh.serve(peer, incoming).await);
        drop(session.closed().await);
    };
    drop(nodes[0].shards().start(shard("mesh"), main).unwrap());
    let (node, result) = (nodes[1].clone(), Arc::clone(&sent));
    let main = move |tasks: Tasks| async move {
        let pool = create_pool();
        let transport = create_transport(&node, &tasks, &pool, PrivateKey([2; 32]));
        let session = transport.dial(public_key(), &[at]).await.unwrap();
        let mut sender = session.open_sender(Class::Command).await.unwrap();
        for bytes in [&wire::header::encode(Protocol::Mesh)[..], &[0xff]] {
            let mut block = pool.alloc(bytes.len()).unwrap();
            block.copy_from_slice(bytes);
            sender.send(block.freeze()).await.unwrap();
        }
        node.clock().sleep(Span::SECOND).await;
        *result.lock().unwrap() = Some(sender.finish());
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    drop(nodes[1].shards().start(shard("peer"), main).unwrap());
    sim.run().unwrap();
    assert_eq!(*served.lock().unwrap(), Some(Err(Error::Malformed)));
    let stopped = transport::Error::Stopped { code: Code(2) };
    assert_eq!(*sent.lock().unwrap(), Some(Err(stopped)));
}

#[test]
fn watch_member_next_and_serve_have_the_signatures_that_a_caller_holds() {
    let _: fn(&Mesh, channel::Key) -> Watch = Mesh::watch;
    let _: fn(&Mesh, node::Key) -> Option<Member> = Mesh::member;
    assert_gives_a_home(Watch::next);
    assert_serves(Mesh::serve);
}

// The match has no wildcard arm, so a new case of `Error` does not compile here.
#[test]
fn error_has_one_case_for_each_cause_that_a_public_call_gives() {
    let cases = |error: &Error| match error {
        Error::Log(_) => 0,
        Error::Raft(_) => 1,
        Error::Spoofed { .. } => 2,
        Error::NotVoter { .. } => 3,
        Error::PeerNotVoter { .. } => 4,
        Error::Claim(_) => 5,
        Error::NotMember(_) => 6,
        Error::Member(_) => 7,
        Error::WrongKey => 8,
        Error::Pool(_) => 9,
        Error::Malformed => 10,
        Error::Stream(_) => 11,
        Error::Stopped(_) => 12,
    };
    assert_eq!(cases(&Error::Stopped(Stopped::Dropped)), 12);
}

#[test]
fn an_error_of_open_or_serve_names_its_cause() {
    let stray = log::Error::Stray {
        path: PathBuf::from("log/notes"),
    };
    assert_error(&stray);
    let stray = Error::Log(stray);
    assert_error(&stray);
    assert_eq!(
        stray.to_string(),
        "log/notes is in the directory of the log, but it is not the next log file"
    );
    let forged = claim::Error::Forged { signer: OTHER };
    assert_error(&forged);
    assert_eq!(
        Error::Claim(forged).to_string(),
        "the claim of node 00000000-0000-0000-0000-000000000002 is forged"
    );
    let duplicate = region::Unfit::Duplicate { key: KEY };
    assert_error(&duplicate);
    assert_eq!(
        Error::Member(duplicate).to_string(),
        "node 00000000-0000-0000-0000-000000000001 is already a member"
    );
    assert_eq!(
        Error::Stopped(Stopped::Dropped).to_string(),
        "the group stopped: each mesh of the group dropped"
    );
}

#[test]
fn a_stop_names_its_cause() {
    let at = Position {
        term: Term(2),
        index: 3,
    };
    let unknown = Stopped::Change {
        at,
        cause: change::Unknown::Kind { kind: 9 },
    };
    assert_error(&unknown);
    assert_eq!(
        unknown.to_string(),
        "the committed entry at index 3 of term 2 is not a change: change kind 9 is \
         unknown"
    );
    let empty = Stopped::Change {
        at,
        cause: change::Unknown::Empty,
    };
    assert_eq!(
        empty.to_string(),
        "the committed entry at index 3 of term 2 is not a change: a change of 0 bytes \
         has no kind"
    );
    let write = Stopped::Write(log::Error::Corrupt {
        path: PathBuf::from("log-0"),
        offset: 512,
    });
    assert_eq!(
        write.to_string(),
        "the record at byte 512 of log-0 is not valid, and it is not a torn end of the \
         log"
    );
    assert_eq!(
        Stopped::Dropped.to_string(),
        "each mesh of the group dropped"
    );
}
