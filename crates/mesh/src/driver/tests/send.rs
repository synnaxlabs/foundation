//! Tests of the tasks of `Mesh::open` that send. Node 1 runs a mesh with nodes 2
//! and 3 as its other voters. Node 2 reads raw streams from its transport, and no
//! node has the address of node 3.

use std::future::pending;

use transport::stream::{Incoming, Receiver, Sender};
use transport::{Class, Code, Session};

use super::*;

/// The largest message that node 2 takes, when a test sets no other.
const LIMIT: usize = 1 << 16;

/// Node 2, with its transport.
struct Peer {
    node: sim::node::Node,
    transport: Transport,
    pool: Rc<Pool>,
}

impl Peer {
    /// The next session that node 1 opens.
    async fn session(&self) -> Session {
        let session = self.transport.accept().await.unwrap();
        assert_eq!(session.peer(), transport::Peer::Node(public(1)));
        session
    }

    /// Whether node 1 opens one more stream of `session`, and whether it opens one
    /// more session, in 3 s.
    async fn more(&self, session: &Session) -> [bool; 2] {
        let mut stream = pin!(session.accept());
        let mut other = pin!(self.transport.accept());
        let mut end = self.node.clock().sleep(seconds(3));
        poll_fn(|cx| {
            let opened = [
                stream.as_mut().poll(cx).is_ready(),
                other.as_mut().poll(cx).is_ready(),
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

    fn block(&self, bytes: &[u8]) -> block::Block {
        let mut block = self.pool.alloc(bytes.len()).unwrap();
        block.copy_from_slice(bytes);
        block.freeze()
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
fn create_config(node: &sim::node::Node, tasks: &Tasks, pool: Rc<Pool>) -> Config {
    Config {
        members: IDS.map(create_voter).into(),
        transport: Rc::new(create_transport(node, tasks, 1, PORT, create_pool())),
        pool,
        ..config(node, tasks, 1, &IDS, &IDS)
    }
}

/// Runs `mesh` on node 1 and `peer` on node 2 for 30 s, and gives what `peer`
/// returned. Node 2 takes a message of at most `limit` bytes.
fn run<M, P>(
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
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let node = nodes[0].clone();
    let main = move |tasks: Tasks| mesh(node, tasks);
    drop(nodes[0].shards().start(shard("mesh"), main).unwrap());
    let read = Arc::new(Mutex::new(None));
    let (node, result) = (nodes[1].clone(), Arc::clone(&read));
    let main = move |tasks: Tasks| async move {
        let pool = create_pool();
        let config = transport::Config {
            message_bytes_max: NonZeroUsize::new(limit).unwrap(),
            ..transport_config(&node, &tasks, 2, Rc::clone(&pool))
        };
        let transport = bind(&node, PORT, config);
        let side = Peer {
            node,
            transport,
            pool,
        };
        let output = peer(side).await;
        *result.lock().unwrap() = Some(output);
    };
    drop(nodes[1].shards().start(shard("peer"), main).unwrap());
    sim.run_for(seconds(30)).unwrap();
    let output = read.lock().unwrap().take();
    output.expect("the peer did not return in 30 s")
}

/// Holds the mesh of node 1 open.
async fn hold(node: sim::node::Node, tasks: Tasks) {
    let config = create_config(&node, &tasks, create_pool());
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

#[test]
fn a_message_that_the_pool_has_no_block_for_drops_and_its_stream_stays() {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let pool = small_pool();
        let config = create_config(&node, &tasks, Rc::clone(&pool));
        let _mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        clock.sleep(seconds(2)).await;
        let blocks = fill(&pool);
        clock.sleep(seconds(4)).await;
        drop(blocks);
        pending::<()>().await;
    };
    let (first, dropped, second, more) = run(LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let first = next(&mut receiver).await;
        let clock = peer.node.clock();
        let waited = within(&clock, seconds(4), pin!(next(&mut receiver))).await;
        let second = next(&mut receiver).await;
        (first, waited.is_none(), second, peer.more(&session).await)
    });
    assert_eq!([first, second], [(); 2].map(|()| Ok(Some(pre_vote()))));
    assert!(dropped, "a message came while the pool had no block");
    assert_eq!(more, [false; 2]);
}

/// Whether node 2, which takes a message of at most `limit` bytes, got the entry of
/// 2000 bytes that node 1 proposed as the leader, and what `Peer::more` gave after 16
/// messages.
fn large(limit: usize) -> (bool, [bool; 2]) {
    let mesh = |node: sim::node::Node, tasks: Tasks| async move {
        let config = create_config(&node, &tasks, create_pool());
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
        (got, peer.more(&session).await)
    })
}

#[test]
fn a_message_that_is_too_large_for_the_peer_drops_and_its_stream_stays() {
    assert_eq!(large(LIMIT), (true, [false; 2]));
    assert_eq!(large(1472), (false, [false; 2]));
}

/// Asserts that node 2 gets one message, and that its stream and its session then
/// end with code 0, when `mesh` runs on node 1.
fn assert_ends<M: Future<Output = ()> + 'static>(
    mesh: impl FnOnce(sim::node::Node, Tasks) -> M + Send + 'static,
) {
    let (sent, closed) = run(LIMIT, mesh, |peer| async move {
        let session = peer.session().await;
        let mut receiver = stream(&session).await;
        let sent = [next(&mut receiver).await, next(&mut receiver).await];
        (sent, session.closed().await)
    });
    assert_eq!(sent, [Ok(Some(pre_vote())), Err(closed.clone())]);
    assert_eq!(closed, transport::Error::PeerClosed { code: Code(0) });
}

/// Asserts that each task that sends ended, also the task of node 3, which waits in
/// a dial: only the test holds `transport`, so only those tasks can end the session.
async fn assert_ended(clock: &Clock, transport: Rc<Transport>) {
    clock.sleep(Span::MILLISECOND).await;
    assert_eq!(Rc::strong_count(&transport), 1, "a task that sends runs");
    pending::<()>().await;
}

#[test]
fn each_task_that_sends_ends_when_the_mesh_drops() {
    assert_ends(|node, tasks| async move {
        let config = create_config(&node, &tasks, create_pool());
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        node.clock().sleep(seconds(2)).await;
        drop(mesh);
        assert_ended(&node.clock(), transport).await;
    });
}

#[test]
fn each_task_that_sends_ends_when_the_group_stops() {
    assert_ends(|node, tasks| async move {
        let config = create_config(&node, &tasks, create_pool());
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        node.clock().sleep(seconds(2)).await;
        fail_sync(&node);
        let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
        mesh.receive(public(2), heartbeat).unwrap();
        assert_ended(&node.clock(), transport).await;
    });
}
