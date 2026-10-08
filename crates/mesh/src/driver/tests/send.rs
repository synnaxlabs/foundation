//! Tests of the tasks of `Mesh::open` that send. Node 1 runs a mesh with nodes 2
//! and 3 as its other voters. Node 2 reads raw streams from its transport, and no
//! node has the address of node 3.

use std::future::pending;

use transport::Class;
use transport::stream::{Incoming, Receiver, Sender};

use super::*;

/// The largest message that node 2 takes, when a test sets no other.
pub(super) const LIMIT: usize = 1 << 16;

/// A peer of node 1, with its transport.
pub(super) struct Peer {
    pub(super) node: sim::node::Node,
    pub(super) tasks: Tasks,
    pub(super) transport: Transport,
    pub(super) pool: Rc<Pool>,
}

impl Peer {
    /// The next session that node 1 opens.
    pub(super) async fn session(&self) -> Session {
        let session = self.transport.accept().await.unwrap();
        assert_eq!(session.peer(), transport::Peer::Node(public(1)));
        session
    }

    /// Whether node 1 opens one more stream of `session`, and whether it opens one
    /// more session, in 3 s.
    ///
    /// # Panics
    ///
    /// When `session` or the transport fails in that time.
    async fn more(&self, session: &Session) -> [bool; 2] {
        let mut stream = pin!(session.accept());
        let mut other = pin!(self.transport.accept());
        let mut end = self.node.clock().sleep(seconds(3));
        poll_fn(|cx| {
            let opened = [
                stream.as_mut().poll(cx).map(Result::unwrap).is_ready(),
                other.as_mut().poll(cx).map(Result::unwrap).is_ready(),
            ];
            if opened != [false; 2] {
                return Poll::Ready(opened);
            }
            Pin::new(&mut end).poll(cx).map(|()| opened)
        })
        .await
    }

    /// Grants each campaign of node 1, on a stream to it, and takes the first append
    /// that node 1 then sends on `receiver`. Node 1 gets each answer while the
    /// stream that this gives lives.
    async fn elect(&self, receiver: &mut Receiver) -> Sender {
        let addresses = [Address::Udp(address(1))];
        let session = self.transport.dial(public(1), &addresses).await.unwrap();
        let mut sender = session.open_sender(Class::Command).await.unwrap();
        let header = self.block(&wire::header::encode(Protocol::Mesh));
        sender.send(header).await.unwrap();
        let mut leads = false;
        while !leads {
            let sent = next(receiver).await.unwrap().unwrap();
            let answer = Answer::Granted(None);
            let body = match sent.body {
                Body::PreVote { .. } => Body::PreVoteReply { answer },
                Body::Vote { .. } => Body::VoteReply { answer },
                Body::Append { entries, .. } => {
                    leads = true;
                    let last = entries.last().unwrap().at.index;
                    Body::AppendReply { last }
                }
                other => panic!("node 1 sent {other:?} before it led"),
            };
            let mut ready = Ready {
                messages: vec![raft::Message {
                    term: sent.term,
                    ..message(2, 1, body)
                }],
                ..Ready::default()
            };
            common::signer(2).sign(&mut ready);
            let reply = Message::Raft(ready.messages.remove(0)).encode();
            sender.send(self.block(&reply)).await.unwrap();
        }
        sender
    }

    pub(super) fn block(&self, bytes: &[u8]) -> block::Block {
        crate::bytes::block(&self.pool, bytes).unwrap()
    }
}

/// The next stream of `session`, after its header.
async fn stream(session: &Session) -> Receiver {
    let Incoming {
        class,
        mut receiver,
        sender,
    } = session.accept().await.unwrap();
    assert_eq!(class, Class::Command);
    assert!(
        sender.is_none(),
        "node 1 opened a stream that goes both ways"
    );
    let header = receiver.recv().await.unwrap().unwrap();
    let protocol = wire::header::decode(&header).unwrap();
    assert_eq!(protocol, (Protocol::Mesh, &[][..]));
    receiver
}

/// The next message of `receiver`.
async fn next(
    receiver: &mut Receiver,
) -> Result<Option<raft::Message>, transport::Error> {
    let bytes = receiver.recv().await?;
    Ok(bytes.map(|bytes| match Message::decode(&bytes) {
        Some(Message::Raft(message)) => message,
        other => panic!("node 1 sent {other:?}"),
    }))
}

/// The config of the mesh of node 1, which sends with `pool`.
pub(super) async fn create_config(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: Rc<Pool>,
) -> Config {
    Config {
        pool,
        ..dialed_at(node, tasks, 1, PORT, &IDS, &IDS).await
    }
}

/// Runs `mesh` on node 1 and `peer` on node 2 for 30 s, and gives what `peer`
/// returned. Node 2 takes a message of at most `limit` bytes, and node 1 sends at
/// most `limit` bytes that node 2 did not read.
pub(super) fn run<M, P>(
    limit: usize,
    mesh: impl FnOnce(sim::node::Node, Tasks) -> M + Send + 'static,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> P::Output
where
    M: Future<Output = ()> + 'static,
    P: Future<Output: Send + 'static> + 'static,
{
    run_as(2, limit, mesh, peer)
}

/// As [`run`], with the keys of node `id` at the address of node 2.
fn run_as<M, P>(
    id: u8,
    limit: usize,
    mesh: impl FnOnce(sim::node::Node, Tasks) -> M + Send + 'static,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> P::Output
where
    M: Future<Output = ()> + 'static,
    P: Future<Output: Send + 'static> + 'static,
{
    let mut sim = Sim::new(sim::Config::default());
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    start(&nodes[0], "mesh", mesh);
    let read = start_peer(&nodes[1], id, limit, peer);
    sim.run_for(seconds(30)).unwrap();
    let output = read.lock().unwrap().take();
    output.expect("the peer did not return in 30 s")
}

/// Starts `main` on a shard of `node`.
fn start<M: Future<Output = ()> + 'static>(
    node: &sim::node::Node,
    name: &str,
    main: impl FnOnce(sim::node::Node, Tasks) -> M + Send + 'static,
) {
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    let inner = node.clone();
    let main = move |tasks: Tasks| main(inner, tasks);
    drop(node.shards().start(config, main).unwrap());
}

/// Starts `peer` on `node` with the keys of node `id`, and gives the place of what it
/// returns.
fn start_peer<P>(
    node: &sim::node::Node,
    id: u8,
    limit: usize,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> Arc<Mutex<Option<P::Output>>>
where
    P: Future<Output: Send + 'static> + 'static,
{
    let read = Arc::new(Mutex::new(None));
    let result = Arc::clone(&read);
    start(node, "peer", move |node, tasks| async move {
        let pool = create_pool();
        let config = transport::Config {
            message_bytes_max: NonZeroUsize::new(limit).unwrap(),
            window_bytes: limit,
            ..transport_config(&node, &tasks, id, Rc::clone(&pool))
        };
        let transport = bind(&node, PORT, config);
        let side = Peer {
            node,
            tasks,
            transport,
            pool,
        };
        let output = peer(side).await;
        *result.lock().unwrap() = Some(output);
    });
    read
}

/// Holds the mesh of node 1 open.
async fn hold(node: sim::node::Node, tasks: Tasks) {
    let config = create_config(&node, &tasks, create_pool()).await;
    let _mesh = Mesh::open(config).await.unwrap();
    pending::<()>().await;
}

/// What node 1 sends to node 2 at each election timeout while no voter answers.
fn pre_vote() -> raft::Message {
    let last = Position::default();
    raft::Message {
        term: Term(1),
        ..message(1, 2, Body::PreVote { last })
    }
}

#[test]
fn each_message_for_a_member_is_one_message_of_one_stream_after_its_header() {
    let (sent, more) = run(LIMIT, hold, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let mut sent = Vec::new();
        for _ in 0..3 {
            sent.push(next(&mut receiver).await);
        }
        (sent, peer.more(&session).await)
    });
    assert_eq!(sent, vec![Ok(Some(pre_vote())); 3]);
    assert_eq!(more, [false; 2]);
}

#[test]
fn a_stream_that_the_peer_stops_gives_way_to_a_new_stream_of_the_same_session() {
    let (sent, more) = run(LIMIT, hold, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let first = next(&mut receiver).await;
        receiver.stop(Code(7));
        let mut receiver = stream(&session).await;
        let second = next(&mut receiver).await;
        ([first, second], peer.more(&session).await)
    });
    assert_eq!(sent, [(); 2].map(|()| Ok(Some(pre_vote()))));
    assert_eq!(more, [false; 2]);
}

#[test]
fn a_session_that_the_peer_closes_gives_way_to_a_new_session() {
    let (sent, more) = run(LIMIT, hold, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let first = next(&mut receiver).await;
        session.close(Code(7));
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let second = next(&mut receiver).await;
        ([first, second], peer.more(&session).await)
    });
    assert_eq!(sent, [(); 2].map(|()| Ok(Some(pre_vote()))));
    assert_eq!(more, [false; 2]);
}

// Node 3 has an address here. The send that finds the failed session of node 2 comes
// before node 3 stops its stream.
#[test]
fn a_session_that_fails_leaves_the_session_to_each_other_member() {
    let mut sim = Sim::new(sim::Config::default());
    let nodes = IDS.map(|_| sim.node(sim::node::Config::default()));
    start(&nodes[0], "mesh", hold);
    drop(start_peer(&nodes[1], 2, LIMIT, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        assert_eq!(next(&mut receiver).await, Ok(Some(pre_vote())));
        session.close(Code(7));
        let _session = peer.session().await;
        pending::<()>().await;
    }));
    let more = start_peer(&nodes[2], 3, LIMIT, |peer| async move {
        let session = peer.session().await;
        let receiver = stream(&session).await;
        peer.node.clock().sleep(seconds(10)).await;
        receiver.stop(Code(7));
        let _receiver = stream(&session).await;
        peer.more(&session).await
    });
    sim.run_for(seconds(30)).unwrap();
    assert_eq!(more.lock().unwrap().take(), Some([false; 2]));
}

#[test]
fn a_message_that_the_pool_has_no_block_for_drops_and_its_stream_stays() {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let pool = small_pool();
        let config = create_config(&node, &tasks, Rc::clone(&pool)).await;
        let _mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        clock.sleep(seconds(4)).await;
        let blocks = fill(&pool);
        clock.sleep(seconds(4)).await;
        drop(blocks);
        pending::<()>().await;
    };
    let (sent, waited, second, more) = run(LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let clock = peer.node.clock();
        let mut sent = Vec::new();
        let mut waited = Some(next(&mut receiver).await);
        // A campaign comes each 2 s at most, so 3 s with no message is a drop.
        while let Some(Ok(Some(message))) = waited {
            sent.push(message);
            waited = within(&clock, seconds(3), pin!(next(&mut receiver))).await;
        }
        let second = next(&mut receiver).await;
        (sent, waited, second, peer.more(&session).await)
    });
    assert_eq!(waited, None, "the stream ended");
    assert_eq!(sent, vec![pre_vote(); sent.len()]);
    assert_eq!(second, Ok(Some(pre_vote())));
    assert_eq!(more, [false; 2]);
}

#[test]
fn no_stream_comes_while_the_pool_has_no_block_for_its_header() {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let pool = small_pool();
        let config = create_config(&node, &tasks, Rc::clone(&pool)).await;
        let _mesh = Mesh::open(config).await.unwrap();
        let blocks = fill(&pool);
        node.clock().sleep(seconds(6)).await;
        drop(blocks);
        pending::<()>().await;
    };
    let (early, first, more) = run(LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let clock = peer.node.clock();
        let early = within(&clock, seconds(2), pin!(session.accept())).await;
        let mut receiver = stream(&session).await;
        let first = next(&mut receiver).await;
        (early.is_some(), first, peer.more(&session).await)
    });
    assert!(!early, "a stream came while the pool had no block");
    assert_eq!(first, Ok(Some(pre_vote())));
    assert_eq!(more, [false; 2]);
}

#[test]
fn a_task_that_waits_in_a_dial_holds_no_block() {
    solo(|node, tasks| async move {
        let pool = small_pool();
        let free = fill(&pool).len();
        let config = create_config(&node, &tasks, Rc::clone(&pool)).await;
        let _mesh = Mesh::open(config).await.unwrap();
        node.clock().sleep(seconds(3)).await;
        assert_eq!(fill(&pool).len(), free);
    });
}

/// A position in the term of the tests.
fn at(index: u64) -> Position {
    Position {
        term: common::TERM,
        index,
    }
}

/// The config of a node 1 with no record of node 4, and the append of node 2 whose
/// voter set names node 4.
async fn create_stranger(
    node: &sim::node::Node,
    tasks: &Tasks,
) -> (Config, raft::Message) {
    let members = vec![create_voter(1), common::member(2), common::member(3)];
    let mut config = config_at(node, tasks, 1, PORT, &IDS, &IDS).await;
    config.founding.members = members;
    let voters = Voters {
        incoming: [1, 2, 4].map(key).into(),
        outgoing: IDS.map(key).into(),
    };
    let append = Body::Append {
        prev: Position::default(),
        entries: vec![common::change(2, at(1), voters)],
        commit: 0,
    };
    (config, proven(2, 1, append))
}

// The case of the test after this one: node 1 has a message for node 4 and no record
// of it.
#[test]
fn a_node_queues_a_message_for_a_voter_with_no_member_record() {
    solo(|node, tasks| async move {
        let (config, append) = create_stranger(&node, &tasks).await;
        let mesh = Mesh::start(config).await.unwrap();
        assert_eq!(mesh.receive(public(2), append), Ok(()));
        let clock = node.clock();
        let sent = within(&clock, seconds(10), pin!(mesh.outgoing(key(4)))).await;
        let to = sent.map(|message| message.map(|message| message.to));
        assert_eq!(to, Some(Ok(key(4))));
        assert_eq!(mesh.member(key(4)), None);
    });
}

// Node 1 has no record of node 4 until 10 s after a voter set names it. Node 4 then
// joins with the address of node 2, so only a task that went on reaches it.
#[test]
fn a_message_for_a_node_with_no_member_record_drops_and_its_task_goes_on() {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let (config, append) = create_stranger(&node, &tasks).await;
        let mesh = Mesh::open(config).await.unwrap();
        assert_eq!(mesh.receive(public(2), append), Ok(()));
        let clock = node.clock();
        clock.sleep(seconds(10)).await;
        assert_eq!(mesh.member(key(4)), None);
        let mut card = common::member(4).card.card().clone();
        let addresses = vec![Address::Udp(address(2))];
        card.addresses = card::addresses::Addresses::new(addresses).unwrap();
        let card = card::Signed::sign(key(4), card, &private(4));
        let join = join_with(&card, 7, Stamp::EPOCH);
        let entry = |index, data| Entry {
            at: at(index),
            data,
        };
        let append = Body::Append {
            prev: at(1),
            entries: vec![
                entry(2, Data::Bytes(encoded(&ticket()))),
                entry(3, Data::Bytes(encoded(&join))),
            ],
            commit: 3,
        };
        assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
        pending::<()>().await;
    };
    let first = run_as(4, LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        next(&mut receiver).await.unwrap().map(|message| message.to)
    });
    assert_eq!(first, Some(key(4)));
}

/// Whether node 2, which takes a message of at most `limit` bytes, got the entry of
/// 2000 bytes that node 1 proposed as the leader, and what `Peer::more` gave after 16
/// messages, and again after node 2 stopped the stream and got a new one.
fn large(limit: usize) -> (bool, [[bool; 2]; 2]) {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let config = create_config(&node, &tasks, create_pool()).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let (serving, streams) = (mesh.clone(), tasks.clone());
        tasks.spawn(async move {
            accept(serving, transport, streams).await;
        });
        lead(&mesh, &node.clock(), home(1)).await;
        mesh.propose_data(vec![0; 2000]).await.unwrap();
        pending::<()>().await;
    };
    run(limit, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let _replies = peer.elect(&mut receiver).await;
        let mut got = false;
        for _ in 0..16 {
            let sent = next(&mut receiver).await.unwrap().unwrap();
            let Body::Append { entries, .. } = sent.body else {
                continue;
            };
            let large = |entry: &Entry| match &entry.data {
                Data::Bytes(bytes) => bytes.len() == 2000,
                Data::Empty | Data::Voters(_) => false,
            };
            got |= entries.iter().any(large);
        }
        let quiet = peer.more(&session).await;
        receiver.stop(Code(7));
        let _receiver = stream(&session).await;
        (got, [quiet, peer.more(&session).await])
    })
}

#[test]
fn a_message_that_is_too_large_for_the_peer_drops_and_its_stream_stays() {
    assert_eq!(large(LIMIT), (true, [[false; 2]; 2]));
    assert_eq!(large(1472), (false, [[false; 2]; 2]));
}

/// Asserts that node 2 gets a message, and that its stream then resets with code 0,
/// when `mesh` runs on node 1.
fn assert_ends<M: Future<Output = ()> + 'static>(
    mesh: impl FnOnce(sim::node::Node, Tasks) -> M + Send + 'static,
) {
    let (sent, end) = run(LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let mut sent = Vec::new();
        let end = loop {
            match next(&mut receiver).await {
                Ok(Some(message)) => sent.push(message),
                end => break end,
            }
        };
        (sent, end)
    });
    assert!(!sent.is_empty(), "no message came before the end");
    assert_eq!(sent, vec![pre_vote(); sent.len()]);
    assert_eq!(end, Err(transport::Error::Reset { code: Code(0) }));
}

/// Asserts that each task that sends ended, also the task of node 3, which waits in
/// a dial: only the test holds `transport`, so only those tasks can end the session.
async fn assert_ended(clock: &Clock, transport: Rc<Transport>) {
    clock.sleep(Span::MILLISECOND).await;
    assert_eq!(Rc::strong_count(&transport), 1, "a task that sends runs");
    pending::<()>().await;
}

/// Stops the group of `mesh`, and gives why: the write of the term of a heartbeat
/// fails.
pub(super) fn stop(node: &sim::node::Node, mesh: &Mesh) -> Stopped {
    let stopped = fail_sync(node);
    let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
    mesh.receive(public(2), heartbeat).unwrap();
    stopped
}

#[test]
fn each_task_that_sends_ends_when_the_mesh_drops() {
    assert_ends(|node, tasks| async move {
        let config = create_config(&node, &tasks, create_pool()).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        node.clock().sleep(seconds(3)).await;
        drop(mesh);
        assert_ended(&node.clock(), transport).await;
    });
}

#[test]
fn each_task_that_sends_ends_when_the_group_stops() {
    assert_ends(|node, tasks| async move {
        let config = create_config(&node, &tasks, create_pool()).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        node.clock().sleep(seconds(3)).await;
        stop(&node, &mesh);
        assert_ended(&node.clock(), transport).await;
    });
}

// The reply to the append is in the queue of node 2 at the stop, so that queue has
// no waker then.
#[test]
fn each_task_that_sends_ends_when_a_committed_entry_stops_the_group() {
    assert_ends(|node, tasks| async move {
        let config = create_config(&node, &tasks, create_pool()).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        node.clock().sleep(seconds(3)).await;
        let at = Position {
            term: Term(5),
            index: 1,
        };
        let entry = Entry {
            at,
            data: Data::Bytes(vec![9]),
        };
        let append = Body::Append {
            prev: Position::default(),
            entries: vec![entry],
            commit: 1,
        };
        mesh.receive(public(2), proven(2, 1, append)).unwrap();
        let cause = Unknown::Kind { kind: 9 };
        assert_eq!(watch.next().await, Err(Stopped::Change { at, cause }));
        drop(watch);
        assert_ended(&node.clock(), transport).await;
    });
}

/// Asserts that the session of node 2 ends with code 0 when `end` ran on the mesh
/// of node 1, whose task for node 2 waits in a send then: node 2 reads nothing
/// after the first append of node 1.
fn assert_ends_in_a_send<E: Future<Output = ()> + 'static>(
    end: impl FnOnce(sim::node::Node, Mesh) -> E + Send + 'static,
) {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let config = create_config(&node, &tasks, create_pool()).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        {
            // Node 1 serves node 2 only until it leads: no task holds the mesh after.
            let mut serving = pin!(async {
                serve_first(&mesh, &transport).await;
                pending::<std::convert::Infallible>().await
            });
            let mut leading = pin!(lead(&mesh, &clock, home(1)));
            poll_fn(|cx| {
                let Poll::Pending = serving.as_mut().poll(cx);
                leading.as_mut().poll(cx)
            })
            .await;
        }
        clock.sleep(seconds(10)).await;
        for _ in 0..2 {
            assert!(!quiet(&mesh, 2).await, "the task of node 2 does not wait");
        }
        end(node, mesh).await;
        pending::<()>().await;
    };
    let closed = run(1472, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        drop(peer.elect(&mut receiver).await);
        session.closed().await
    });
    assert_eq!(closed, transport::Error::PeerClosed { code: Code(0) });
}

#[test]
fn a_task_that_waits_in_a_send_ends_when_the_mesh_drops() {
    assert_ends_in_a_send(|_, mesh| async move { drop(mesh) });
}

#[test]
fn a_task_that_waits_in_a_send_ends_when_the_group_stops() {
    assert_ends_in_a_send(|node, mesh| async move {
        stop(&node, &mesh);
        pending::<()>().await;
    });
}

/// Serves the first stream that node 2 opens to `mesh`, on the first session that
/// `transport` accepts.
async fn serve_first(mesh: &Mesh, transport: &Transport) {
    let session = transport.accept().await.unwrap();
    let mut incoming = session.accept().await.unwrap();
    let Ok(Some(_)) = incoming.receiver.recv().await else {
        return;
    };
    drop(mesh.serve(public(2), incoming).await);
}

// Serves each stream that node 2 opens to `mesh`.
fn serve_each(mesh: &Mesh, tasks: &Tasks, transport: Rc<Transport>) {
    let (serving, streams) = (mesh.clone(), tasks.clone());
    tasks.spawn(async move {
        let session = transport.accept().await.unwrap();
        while let Ok(mut incoming) = session.accept().await {
            let mesh = serving.clone();
            streams.spawn(async move {
                let Ok(Some(_)) = incoming.receiver.recv().await else {
                    return;
                };
                drop(mesh.serve(public(2), incoming).await);
            });
        }
    });
}

impl Peer {
    /// Keeps node 1 the leader with a heartbeat reply at each tick, reads nothing
    /// more from it, and stops its stream with `code` at tick `stop`.
    async fn follow(
        &self,
        replies: &mut Sender,
        receiver: Receiver,
        stop: u32,
        code: Code,
    ) {
        let mut receiver = Some(receiver);
        let clock = self.node.clock();
        for tick in 0..400 {
            if tick == stop {
                receiver.take().unwrap().stop(code);
            }
            let mut ready = Ready {
                messages: vec![raft::Message {
                    term: Term(1),
                    ..message(2, 1, Body::HeartbeatReply)
                }],
                ..Ready::default()
            };
            common::signer(2).sign(&mut ready);
            let reply = Message::Raft(ready.messages.remove(0)).encode();
            if replies.send(self.block(&reply)).await.is_err() {
                return;
            }
            clock.sleep(TICK).await;
        }
    }
}

// Node 1 leads, and its task for node 2 waits in a send. Its proposal waits for a
// block of the pool when voter 2 answers `removed`. The group stops while the write
// of the entry waits, so the proposal gives that stop.
#[test]
fn a_proposal_whose_write_waits_gives_a_removed_stop() {
    let outcome = Arc::new(Mutex::new(None));
    let written = Arc::clone(&outcome);
    let mesh = move |node: sim::node::Node, tasks: Tasks| async move {
        let pool = small_pool();
        let config = create_config(&node, &tasks, Rc::clone(&pool)).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        serve_each(&mesh, &tasks, transport);
        lead(&mesh, &clock, home(1)).await;
        clock.sleep(seconds(10)).await;
        for _ in 0..2 {
            assert!(!quiet(&mesh, 2).await, "the task of node 2 does not wait");
        }
        let _blocks = fill(&pool);
        let mut call = pin!(mesh.propose_data(vec![1]));
        assert!(now(call.as_mut()).await.is_pending());
        clock.sleep(seconds(2)).await;
        assert!(
            now(call.as_mut()).await.is_pending(),
            "the write did not wait"
        );
        assert_eq!(mesh.propose_data(vec![2]).await, Err(exhausted(61)));
        let proposed = within(&clock, seconds(20), call).await;
        let watched = mesh.watch(INDEX).next().await;
        *written.lock().unwrap() = Some((proposed, watched));
        pending::<()>().await;
    };
    let mut sim = Sim::new(sim::Config::default());
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    start(&nodes[0], "mesh", mesh);
    drop(start_peer(&nodes[1], 2, 1472, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let mut replies = peer.elect(&mut receiver).await;
        peer.follow(&mut replies, receiver, 150, Code(17)).await;
        pending::<()>().await;
    }));
    sim.run_for(seconds(90)).unwrap();
    let removed = Stopped::Removed { by: key(2) };
    let expected = (Some(Err(Error::Stopped(removed.clone()))), Err(removed));
    assert_eq!(outcome.lock().unwrap().take(), Some(expected));
}
