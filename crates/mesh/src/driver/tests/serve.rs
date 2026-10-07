//! Tests of `Mesh::serve`. Node 2 writes raw streams to node 1 over two transports,
//! and node 1 serves each with its mesh.

use transport::stream::{Incoming, Receiver, Sender};
use transport::{Class, Code, Session};

use super::*;

/// The code of a stream that carried a message that is not valid for it.
const MALFORMED: Code = Code(2);
/// The code of a stream with a message that the mesh refused.
const REFUSED: Code = Code(16);

/// Node 2, with its session to node 1.
struct Peer {
    node: sim::node::Node,
    session: Session,
    pool: Rc<Pool>,
}

impl Peer {
    async fn send(&self, sender: &mut Sender, bytes: &[u8]) {
        let mut block = self.pool.alloc(bytes.len()).unwrap();
        block.copy_from_slice(bytes);
        sender.send(block.freeze()).await.unwrap();
    }

    /// Sends the header of the mesh protocol, then `messages`.
    async fn start(&self, sender: &mut Sender, messages: &[Vec<u8>]) {
        self.send(sender, &wire::header::encode(Protocol::Mesh))
            .await;
        for message in messages {
            self.send(sender, message).await;
        }
    }

    /// Opens a stream that only the peer sends on, and starts it with `messages`.
    async fn send_each(&self, messages: &[Vec<u8>]) -> Sender {
        let mut sender = self.session.open_sender(Class::Command).await.unwrap();
        self.start(&mut sender, messages).await;
        sender
    }

    /// Opens a stream that goes both ways, and starts it with `messages`.
    async fn ask(&self, messages: &[Vec<u8>]) -> (Sender, Receiver) {
        let (mut sender, receiver) = self.session.open(Class::Command).await.unwrap();
        self.start(&mut sender, messages).await;
        (sender, receiver)
    }

    /// Waits until node 1 served what the peer sent.
    async fn settle(&self) {
        self.node.clock().sleep(seconds(1)).await;
    }
}

/// The next message of `receiver`.
async fn next(receiver: &mut Receiver) -> Result<Option<Message>, transport::Error> {
    let bytes = receiver.recv().await?;
    Ok(bytes.map(|bytes| Message::decode(&bytes).unwrap()))
}

/// Runs `mesh` on node 1 with the first stream that node 2 opens, after its header,
/// and `peer` on node 2. The session ends after `peer` returns. Gives what each
/// returned.
fn run<M, P>(
    mesh: impl FnOnce(sim::node::Node, Tasks, Incoming) -> M + Send + 'static,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> (M::Output, P::Output)
where
    M: Future<Output: Send + 'static> + 'static,
    P: Future<Output: Send + 'static> + 'static,
{
    let mut sim = Sim::new(sim::Config::default());
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = SocketAddr::new(nodes[0].addresses()[0], PORT);
    let served = Arc::new(Mutex::new(None));
    let sent = Arc::new(Mutex::new(None));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let (node, result) = (nodes[0].clone(), Arc::clone(&served));
    let main = move |tasks: Tasks| async move {
        let transport = create_transport(&node, &tasks, 1, PORT, create_pool());
        let session = transport.accept().await.unwrap();
        let mut incoming = session.accept().await.unwrap();
        let header = incoming.receiver.recv().await.unwrap().unwrap();
        let protocol = wire::header::decode(&header).unwrap();
        assert_eq!(protocol, (Protocol::Mesh, &[][..]));
        let output = mesh(node, tasks, incoming).await;
        *result.lock().unwrap() = Some(output);
        drop(session.closed().await);
    };
    drop(nodes[0].shards().start(shard("mesh"), main).unwrap());
    let (node, result) = (nodes[1].clone(), Arc::clone(&sent));
    let main = move |tasks: Tasks| async move {
        let pool = create_pool();
        let transport = create_transport(&node, &tasks, 2, PORT, Rc::clone(&pool));
        let addresses = [Address::Udp(at)];
        let session = transport.dial(public(1), &addresses).await.unwrap();
        let clock = node.clock();
        let side = Peer {
            node,
            session: session.clone(),
            pool,
        };
        let output = peer(side).await;
        *result.lock().unwrap() = Some(output);
        session.close(Code(0));
        // The close goes out.
        clock.sleep(Span::MILLISECOND).await;
    };
    drop(nodes[1].shards().start(shard("peer"), main).unwrap());
    sim.run().unwrap();
    let outputs = (served.lock().unwrap().take(), sent.lock().unwrap().take());
    (outputs.0.unwrap(), outputs.1.unwrap())
}

fn heartbeat() -> raft::Message {
    proven(2, 1, Body::Heartbeat { commit: 0 })
}

fn raft(message: raft::Message) -> Vec<u8> {
    Message::Raft(message).encode()
}

fn propose(id: u8) -> Vec<u8> {
    Message::Propose { change: home(id) }.encode()
}

/// The mesh of node 1 as the lone voter, once it leads, with the position of its
/// first entry.
async fn leader(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: Rc<Pool>,
) -> (Mesh, Position) {
    let config = Config {
        pool,
        ..config(node, tasks, 1, &[1, 2], &[1])
    };
    let mesh = Mesh::start(config).await.unwrap();
    let first = lead(&mesh, &node.clock(), home(1)).await;
    (mesh, first)
}

#[test]
fn serve_gives_the_group_each_message_of_a_one_way_stream() {
    let (served, finished) = run(
        |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let served = mesh.serve(public(2), incoming).await;
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            assert!(quiet(&mesh, 2).await);
            served
        },
        |peer| async move {
            let messages = [raft(heartbeat()), raft(heartbeat())];
            let mut sender = peer.send_each(&messages).await;
            let finished = sender.finish();
            peer.settle().await;
            finished
        },
    );
    assert_eq!((served, finished), (Ok(()), Ok(())));
}

#[test]
fn serve_stops_a_one_way_stream_at_the_first_message_that_the_group_refuses() {
    let mut forged = heartbeat();
    let proof = forged.proof.as_mut().unwrap();
    proof.voters.get_mut(&key(3)).unwrap().as_mut().unwrap().0[63] ^= 1;
    let stranger = message(4, 1, Body::Heartbeat { commit: 0 });
    let misrouted = message(2, 3, Body::HeartbeatReply);
    let cases = [
        (3, heartbeat(), Error::Spoofed { from: key(2) }),
        (4, stranger, Error::NotVoter { from: key(4) }),
        (
            2,
            forged,
            Error::Grant(grant::Error::Forged { voter: key(3) }),
        ),
        (
            2,
            misrouted,
            Error::Raft(raft::Error::Misrouted { to: key(3) }),
        ),
    ];
    for (from, refused, error) in cases {
        let (served, finished) = run(
            move |node, tasks, incoming| async move {
                let config = config(&node, &tasks, 1, &[1, 2, 3, 4], &IDS);
                let mesh = Mesh::start(config).await.unwrap();
                let served = mesh.serve(public(from), incoming).await;
                // The group did not see the heartbeat after the refused message:
                // its reply comes after the write of the term.
                node.clock().sleep(seconds(1)).await;
                assert!(quiet(&mesh, 2).await);
                served
            },
            |peer| async move {
                let messages = [raft(refused), raft(heartbeat())];
                let mut sender = peer.send_each(&messages).await;
                peer.settle().await;
                sender.finish()
            },
        );
        let stopped = transport::Error::Stopped { code: REFUSED };
        assert_eq!((served, finished), (Err(error), Err(stopped)));
    }
}

#[test]
fn serve_stops_a_one_way_stream_at_a_message_that_it_does_not_carry() {
    let at = Position {
        term: Term(1),
        index: 1,
    };
    let cases = [
        vec![0xff],
        Vec::new(),
        propose(3),
        Message::Proposed { at }.encode(),
        Message::NotLeader { leader: None }.encode(),
    ];
    for bytes in cases {
        let (served, finished) = run(
            |node, tasks, incoming| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let served = mesh.serve(public(2), incoming).await;
                node.clock().sleep(seconds(1)).await;
                assert!(quiet(&mesh, 2).await);
                served
            },
            |peer| async move {
                let messages = [bytes, raft(heartbeat())];
                let mut sender = peer.send_each(&messages).await;
                peer.settle().await;
                sender.finish()
            },
        );
        let stopped = transport::Error::Stopped { code: MALFORMED };
        assert_eq!((served, finished), (Err(Error::Malformed), Err(stopped)));
    }
}

// The write of the term waits for a block when the second heartbeat comes, so the
// group does not take it. `raft` sends such a message again, so the stream goes on.
#[test]
fn serve_drops_a_message_that_the_pool_has_no_block_for_and_goes_on() {
    let (served, finished) = run(
        |node, tasks, incoming| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS)
            };
            let mesh = Mesh::start(config).await.unwrap();
            let held = fill(&pool);
            let free = {
                let clock = node.clock();
                async move {
                    clock.sleep(seconds(2)).await;
                    drop(held);
                }
            };
            tasks.spawn(free);
            let served = mesh.serve(public(2), incoming).await;
            for _ in 0..2 {
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            }
            assert!(quiet(&mesh, 2).await);
            served
        },
        |peer| async move {
            let mut sender = peer.send_each(&[raft(heartbeat())]).await;
            peer.settle().await;
            peer.send(&mut sender, &raft(heartbeat())).await;
            peer.settle().await;
            peer.settle().await;
            peer.send(&mut sender, &raft(heartbeat())).await;
            let finished = sender.finish();
            peer.settle().await;
            finished
        },
    );
    assert_eq!((served, finished), (Ok(()), Ok(())));
}

#[test]
fn serve_gives_the_cause_when_the_peer_resets_the_stream() {
    let (served, reset) = run(
        |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            mesh.serve(public(2), incoming).await
        },
        |peer| async move {
            let (sender, mut receiver) = peer.ask(&[]).await;
            // Node 1 has the header.
            peer.settle().await;
            sender.reset(Code(40));
            next(&mut receiver).await
        },
    );
    let cause = transport::Error::Reset { code: Code(40) };
    let error = Error::Stream(cause);
    assert_eq!(
        error.to_string(),
        "a mesh stream failed: the peer reset the stream (40)"
    );
    assert_eq!(served, Err(error));
    // A stream that failed gets no code of the mesh.
    assert_eq!(reset, Err(transport::Error::Reset { code: Code(0) }));
}

#[test]
fn serve_gives_the_cause_when_the_peer_resets_a_one_way_stream() {
    let (served, ()) = run(
        |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            mesh.serve(public(2), incoming).await
        },
        |peer| async move {
            let sender = peer.send_each(&[raft(heartbeat())]).await;
            peer.settle().await;
            sender.reset(Code(40));
            peer.settle().await;
        },
    );
    let cause = transport::Error::Reset { code: Code(40) };
    assert_eq!(served, Err(Error::Stream(cause)));
}

#[test]
fn serve_stops_a_one_way_stream_when_the_group_stopped() {
    let ((served, stop), finished) = run(
        |node, tasks, incoming| async move {
            let (mesh, _) = leader(&node, &tasks, create_pool()).await;
            let stop = fail_sync(&node);
            assert_eq!(mesh.propose(home(4)).await, Err(stop.clone()));
            (mesh.serve(public(2), incoming).await, stop)
        },
        |peer| async move {
            let mut sender = peer.send_each(&[raft(heartbeat())]).await;
            // Node 1 leads and stops first.
            peer.node.clock().sleep(seconds(10)).await;
            sender.finish()
        },
    );
    let stopped = transport::Error::Stopped { code: REFUSED };
    assert_eq!((served, finished), (Err(stop), Err(stopped)));
}

#[test]
fn serve_answers_a_proposal_with_its_position_when_the_node_leads() {
    let ((served, first, home), answers) = run(
        |node, tasks, incoming| async move {
            let (mesh, first) = leader(&node, &tasks, create_pool()).await;
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let served = mesh.serve(public(1), incoming).await;
            (served, first, watch.next().await)
        },
        |peer| async move {
            let (mut sender, mut receiver) = peer.ask(&[propose(3)]).await;
            let finished = sender.finish();
            let answer = next(&mut receiver).await;
            (finished, answer, next(&mut receiver).await)
        },
    );
    assert_eq!((served, home), (Ok(()), Ok(Some(key(3)))));
    let answer = Message::Proposed {
        at: after(first, 1),
    };
    assert_eq!(answers, (Ok(()), Ok(Some(answer)), Ok(None)));
}

#[test]
fn serve_answers_a_proposal_with_the_leader_when_the_node_follows() {
    let (served, answers) = run(
        |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            assert_eq!(mesh.receive(public(2), heartbeat()), Ok(()));
            mesh.serve(public(2), incoming).await
        },
        |peer| async move {
            let (mut sender, mut receiver) = peer.ask(&[propose(3)]).await;
            let finished = sender.finish();
            let answer = next(&mut receiver).await;
            (finished, answer, next(&mut receiver).await)
        },
    );
    let leader = Some(key(2));
    let answer = Message::NotLeader { leader };
    assert_eq!(served, Ok(()));
    assert_eq!(answers, (Ok(()), Ok(Some(answer)), Ok(None)));
}

#[test]
fn serve_answers_a_proposal_with_no_leader_when_the_node_knows_none() {
    let (served, answers) = run(
        |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            mesh.serve(public(2), incoming).await
        },
        |peer| async move {
            let (mut sender, mut receiver) = peer.ask(&[propose(3)]).await;
            let finished = sender.finish();
            let answer = next(&mut receiver).await;
            (finished, answer, next(&mut receiver).await)
        },
    );
    let answer = Message::NotLeader { leader: None };
    assert_eq!(served, Ok(()));
    assert_eq!(answers, (Ok(()), Ok(Some(answer)), Ok(None)));
}

#[test]
fn serve_gives_the_cause_when_the_stream_fails_after_the_answer() {
    let ((served, first), answer) = run(
        |node, tasks, incoming| async move {
            let (mesh, first) = leader(&node, &tasks, create_pool()).await;
            (mesh.serve(public(1), incoming).await, first)
        },
        |peer| async move {
            let (sender, mut receiver) = peer.ask(&[propose(3)]).await;
            let answer = next(&mut receiver).await;
            sender.reset(Code(40));
            peer.settle().await;
            (answer, next(&mut receiver).await)
        },
    );
    let at = after(first, 1);
    let cause = transport::Error::Reset { code: Code(40) };
    assert_eq!(served, Err(Error::Stream(cause)));
    assert_eq!(answer, (Ok(Some(Message::Proposed { at })), Ok(None)));
}

// The peer does not read the answer, so the send of the answer fails.
#[test]
fn serve_gives_the_cause_when_the_peer_stopped_the_reply_half() {
    let ((served, next, first), ()) = run(
        |node, tasks, incoming| async move {
            let (mesh, first) = leader(&node, &tasks, create_pool()).await;
            let served = mesh.serve(public(1), incoming).await;
            let next = mesh.propose(home(5)).await;
            (served, next, first)
        },
        |peer| async move {
            let (mut sender, receiver) = peer.ask(&[]).await;
            receiver.stop(Code(40));
            peer.settle().await;
            peer.send(&mut sender, &propose(3)).await;
            sender.finish().unwrap();
            peer.settle().await;
        },
    );
    let cause = transport::Error::Stopped { code: Code(40) };
    assert_eq!(served, Err(Error::Stream(cause)));
    // The group took the change.
    assert_eq!(next, Ok(after(first, 2)));
}

/// What the peer sees on a stream that goes both ways and that node 1 stops: what
/// its receiver gives, then what its sender gives.
async fn stopped(peer: Peer, messages: &[Vec<u8>]) -> [transport::Error; 2] {
    let (mut sender, mut receiver) = peer.ask(messages).await;
    let reset = next(&mut receiver).await.unwrap_err();
    peer.settle().await;
    [reset, sender.finish().unwrap_err()]
}

/// What [`stopped`] gives for a stream that node 1 stops with `code`.
fn codes(code: Code) -> [transport::Error; 2] {
    [
        transport::Error::Reset { code },
        transport::Error::Stopped { code },
    ]
}

#[test]
fn serve_refuses_a_proposal_of_a_peer_that_is_no_voter() {
    let (served, seen) = run(
        |node, tasks, incoming| async move {
            let (mesh, first) = leader(&node, &tasks, create_pool()).await;
            let served = mesh.serve(public(2), incoming).await;
            // The group did not see the change.
            assert_eq!(mesh.propose(home(4)).await, Ok(after(first, 1)));
            served
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    let refused = Error::PeerNotVoter { peer: public(2) };
    assert_eq!((served, seen), (Err(refused), codes(REFUSED)));
}

#[test]
fn serve_refuses_a_proposal_when_the_pool_has_no_block_for_the_answer() {
    let (served, seen) = run(
        |node, tasks, incoming| async move {
            let pool = small_pool();
            let (mesh, first) = leader(&node, &tasks, Rc::clone(&pool)).await;
            let held = fill(&pool);
            let served = mesh.serve(public(1), incoming).await;
            drop(held);
            // The group did not see the change.
            assert_eq!(mesh.propose(home(4)).await, Ok(after(first, 1)));
            served
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    assert_eq!((served, seen), (Err(exhausted(17)), codes(REFUSED)));
}

#[test]
fn serve_refuses_a_proposal_when_the_group_stops() {
    let ((served, stop), seen) = run(
        |node, tasks, incoming| async move {
            let (mesh, _) = leader(&node, &tasks, create_pool()).await;
            let stop = fail_sync(&node);
            (mesh.serve(public(1), incoming).await, stop)
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    assert_eq!((served, seen), (Err(stop), codes(REFUSED)));
}

#[test]
fn serve_stops_a_two_way_stream_that_does_not_start_with_a_proposal() {
    let cases = [vec![0xff], Vec::new(), raft(heartbeat())];
    for bytes in cases {
        let (served, seen) = run(
            |node, tasks, incoming| async move {
                let (mesh, first) = leader(&node, &tasks, create_pool()).await;
                let served = mesh.serve(public(1), incoming).await;
                assert_eq!(mesh.propose(home(4)).await, Ok(after(first, 1)));
                served
            },
            |peer| async move { stopped(peer, &[bytes, propose(3)]).await },
        );
        assert_eq!((served, seen), (Err(Error::Malformed), codes(MALFORMED)));
    }
}

#[test]
fn serve_stops_a_two_way_stream_that_ends_with_no_message() {
    let (served, reset) = run(
        |node, tasks, incoming| async move {
            let (mesh, _) = leader(&node, &tasks, create_pool()).await;
            mesh.serve(public(1), incoming).await
        },
        |peer| async move {
            let (mut sender, mut receiver) = peer.ask(&[]).await;
            sender.finish().unwrap();
            next(&mut receiver).await
        },
    );
    let error = Error::Malformed;
    assert_eq!(error.to_string(), "a message on a mesh stream is not valid");
    let reset = (served, reset);
    let code = MALFORMED;
    assert_eq!(reset, (Err(error), Err(transport::Error::Reset { code })));
}

// The group took the first proposal, and its answer is on the stream.
#[test]
fn serve_stops_a_two_way_stream_at_a_second_message() {
    let ((served, first), (answer, finished)) = run(
        |node, tasks, incoming| async move {
            let (mesh, first) = leader(&node, &tasks, create_pool()).await;
            let served = mesh.serve(public(1), incoming).await;
            // The group did not see the second change.
            assert_eq!(mesh.propose(home(5)).await, Ok(after(first, 2)));
            (served, first)
        },
        |peer| async move {
            let messages = [propose(3), propose(4)];
            let (mut sender, mut receiver) = peer.ask(&messages).await;
            let answer = next(&mut receiver).await;
            peer.settle().await;
            (answer, sender.finish())
        },
    );
    let at = after(first, 1);
    let stopped = transport::Error::Stopped { code: MALFORMED };
    assert_eq!(served, Err(Error::Malformed));
    assert_eq!(answer, Ok(Some(Message::Proposed { at })));
    assert_eq!(finished, Err(stopped));
}
