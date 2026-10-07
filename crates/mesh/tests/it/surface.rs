//! What a crate outside `mesh` opens, reads, and sets of a region, as `node` and
//! `hub` do.

use std::collections::BTreeMap;
use std::future::pending;
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
/// The nodes of a region of three voters. Node `id` has the key of that number.
const IDS: [u8; 3] = [1, 2, 3];
/// The port of each transport.
const PORT: u16 = 7000;

type Home = Result<Option<node::Key>, Stopped>;

fn assert_gives_a_home<'a, F: Future<Output = Home>>(_: fn(&'a mut Watch) -> F) {}

fn assert_serves<'a, F: Future<Output = Result<(), Error>>>(
    _: fn(&'a Mesh, PublicKey, Incoming) -> F,
) {
}

fn assert_sets<'a, F: Future<Output = Result<(), Error>>>(
    _: fn(&'a Mesh, channel::Key, node::Key) -> F,
) {
}

fn assert_error<E: std::error::Error>(_: &E) {}

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn private_key(id: u8) -> PrivateKey {
    PrivateKey([id; 32])
}

fn public_key(id: u8) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&private_key(id).0).unwrap();
    PublicKey::new(pair.public_key().as_ref().try_into().unwrap()).unwrap()
}

/// The record of node `id`, with a card that the node signed and that holds
/// `addresses`.
fn create_member(id: u8, addresses: Vec<Address>) -> Member {
    let card = Card {
        name: format!("plant.node{id}").parse().unwrap(),
        public_key: public_key(id),
        seal_key: SealKey::new([9; 32]).unwrap(),
        addresses: Addresses::new(addresses).unwrap(),
        version: 1,
    };
    Member {
        card: card::Signed::sign(key(id), card, &private_key(id)),
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
    create_voter_config(node, tasks, 1, vec![create_member(1, Vec::new())])
}

/// The config of node `id` of the region `plant`. Each of `members` is a voter.
fn create_voter_config(
    node: &sim::node::Node,
    tasks: &Tasks,
    id: u8,
    members: Vec<Member>,
) -> Config {
    create_config_on(node, tasks, id, members, private_key(id))
}

/// That config, on a transport that proves the public key of `transport_key`.
fn create_config_on(
    node: &sim::node::Node,
    tasks: &Tasks,
    id: u8,
    members: Vec<Member>,
    transport_key: PrivateKey,
) -> Config {
    let pool = create_pool();
    let transport = create_transport(node, tasks, &pool, transport_key);
    Config {
        key: key(id),
        private_key: private_key(id),
        region: "plant".parse::<Prefix>().unwrap(),
        voters: members.iter().map(|member| member.card.key()).collect(),
        members,
        files: node.files(),
        clock: node.clock(),
        time: clock::Clock::new(node.clock()).1,
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
    assert_eq!(run(body), Ok(()));
}

/// What a run of `body` on its one node gives.
fn run<F: Future<Output = ()> + 'static>(
    body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
) -> Result<(), sim::Error> {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, body)
}

/// The public half of `PrivateKey([1; 32])`, as the panic of `open` prints it.
const PUBLIC_1: &str =
    "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";

/// As [`PUBLIC_1`], of `PrivateKey([3; 32])`.
const PUBLIC_3: &str =
    "ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1";

/// The panic of an `open` whose transport proves `proved` and whose config holds the
/// private key of `own`.
fn mismatch(proved: &str, own: &str) -> Result<(), sim::Error> {
    Err(sim::Error::Panicked {
        thread: "run_on".into(),
        message: format!(
            "invariant: the transport of a mesh proves the public half of its private \
             key: it proves {proved}, not {own}"
        ),
        seed: 0,
    })
}

#[test]
fn a_node_opens_its_region_and_reads_its_member_and_a_home() {
    solo(|node, tasks| async move {
        let mesh = Mesh::open(create_config(&node, &tasks)).await.unwrap();
        assert_eq!(mesh.member(KEY), Some(create_member(1, Vec::new())));
        assert_eq!(mesh.member(OTHER), None);
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        drop(mesh);
        assert_eq!(watch.next().await, Err(Stopped::Dropped));
    });
}

#[test]
fn open_panics_on_a_transport_that_proves_another_key() {
    let ran = run(|node, tasks| async move {
        let members = vec![create_member(1, Vec::new())];
        let config = create_config_on(&node, &tasks, 1, members, PrivateKey([3; 32]));
        drop(Mesh::open(config).await);
    });
    assert_eq!(ran, mismatch(PUBLIC_3, PUBLIC_1));
}

// The transport proves the key of the member record, so only the private key of the
// config is the other side of the check.
#[test]
fn open_panics_on_a_private_key_that_its_transport_does_not_prove() {
    let ran = run(|node, tasks| async move {
        let mut config = create_config(&node, &tasks);
        config.private_key = PrivateKey([3; 32]);
        drop(Mesh::open(config).await);
    });
    assert_eq!(ran, mismatch(PUBLIC_1, PUBLIC_3));
}

#[test]
fn open_gives_wrong_key_when_the_transport_proves_the_private_key() {
    solo(|node, tasks| async move {
        let members = vec![create_member(1, Vec::new())];
        let mut config =
            create_config_on(&node, &tasks, 1, members, PrivateKey([3; 32]));
        config.private_key = PrivateKey([3; 32]);
        assert_eq!(Mesh::open(config).await.err(), Some(Error::WrongKey));
    });
}

/// Serves each stream of each session that a peer opens to `transport`, as `node`
/// does.
async fn accept(mesh: Mesh, transport: Rc<Transport>, tasks: Tasks) {
    loop {
        let session = transport.accept().await.unwrap();
        let Peer::Node(peer) = session.peer() else {
            panic!("a peer with no node key opened a session");
        };
        let (mesh, streams) = (mesh.clone(), tasks.clone());
        tasks.spawn(async move {
            while let Ok(mut incoming) = session.accept().await {
                let mesh = mesh.clone();
                streams.spawn(async move {
                    let Ok(Some(header)) = incoming.receiver.recv().await else {
                        return;
                    };
                    let protocol = wire::header::decode(&header).unwrap();
                    assert_eq!(protocol, (Protocol::Mesh, &[][..]));
                    match mesh.serve(peer, incoming).await {
                        Ok(()) | Err(Error::Stream(_)) => {}
                        Err(error) => panic!("a voter refused a message: {error}"),
                    }
                });
            }
        });
    }
}

/// The index whose home node `id` sets.
fn index(id: u8) -> channel::Key {
    channel::Key::from_u128(u128::from(id))
}

// Only one node leads, so at least two of the calls go through the leader.
#[test]
fn each_voter_of_a_region_gets_the_home_that_each_voter_sets() {
    let mut sim = Sim::new(sim::Config::default());
    let nodes = IDS.map(|_| sim.node(sim::node::Config::default()));
    let member = |(id, node): (u8, &sim::node::Node)| {
        let address = SocketAddr::new(node.addresses()[0], PORT);
        create_member(id, vec![Address::Udp(address)])
    };
    let members: Vec<Member> = IDS.into_iter().zip(&nodes).map(member).collect();
    let read = Arc::new(Mutex::new(BTreeMap::new()));
    for (id, node) in IDS.into_iter().zip(&nodes) {
        let (own, members, read) = (node.clone(), members.clone(), Arc::clone(&read));
        let main = move |tasks: Tasks| async move {
            let config = create_voter_config(&own, &tasks, id, members);
            let transport = Rc::clone(&config.transport);
            let mesh = Mesh::open(config).await.unwrap();
            tasks.spawn(accept(mesh.clone(), transport, tasks.clone()));
            let set = mesh.set_home(index(id), key(id)).await;
            let mut homes = Vec::new();
            for of in IDS {
                let mut watch = mesh.watch(index(of));
                let mut home = watch.next().await;
                while home == Ok(None) {
                    home = watch.next().await;
                }
                homes.push(home);
            }
            read.lock().unwrap().insert(id, (set, homes));
            // The other voters need this one until they have each home.
            pending::<()>().await;
        };
        let shard = env::shards::Config {
            name: format!("voter-{id}"),
            core: None,
        };
        drop(node.shards().start(shard, main).unwrap());
    }
    sim.run_for(Span::from_nanos(10 * Span::SECOND.nanos()))
        .unwrap();
    let homes: Vec<_> = IDS.into_iter().map(|id| Ok(Some(key(id)))).collect();
    let expected = IDS.map(|id| (id, (Ok(()), homes.clone()))).into();
    assert_eq!(*read.lock().unwrap(), expected);
}

#[test]
fn a_region_with_two_records_of_one_node_does_not_open() {
    solo(|node, tasks| async move {
        let mut config = create_config(&node, &tasks);
        config.members.push(create_member(1, Vec::new()));
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
        let transport = create_transport(&node, &tasks, &pool, private_key(2));
        let session = transport.dial(public_key(1), &[at]).await.unwrap();
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
fn watch_member_next_serve_and_set_home_have_the_signatures_that_a_caller_holds() {
    let _: fn(&Mesh, channel::Key) -> Watch = Mesh::watch;
    let _: fn(&Mesh, node::Key) -> Option<Member> = Mesh::member;
    assert_gives_a_home(Watch::next);
    assert_serves(Mesh::serve);
    assert_sets(Mesh::set_home);
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
