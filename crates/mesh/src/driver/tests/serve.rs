//! Tests of `Mesh::serve`. Node 2 writes raw streams to node 1 over two transports,
//! and node 1 serves each with its mesh.

use transport::stream::{Incoming, Receiver, Sender};
use transport::{Class, Code};

use super::*;

/// The code of a stream that carried a message that is not valid for it.
const MALFORMED: Code = Code(2);
/// The code of a stream with a message that the mesh refused.
const REFUSED: Code = Code(16);
/// Half of the shortest election timeout.
const HALF: Span = Span::from_nanos(5 * TICK.nanos());

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
    let mesh = |node, tasks, incoming, _| mesh(node, tasks, incoming);
    run_shared(0, create_pool, mesh, peer)
}

/// As [`run`], on run `seed` of the simulation, and the transport of node 1 has the
/// pool that `pool` makes. `mesh` also gets it, so that a mesh can share it as on a
/// shard.
fn run_shared<M, P>(
    seed: u64,
    pool: fn() -> Rc<Pool>,
    mesh: impl FnOnce(sim::node::Node, Tasks, Incoming, Rc<Pool>) -> M + Send + 'static,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> (M::Output, P::Output)
where
    M: Future<Output: Send + 'static> + 'static,
    P: Future<Output: Send + 'static> + 'static,
{
    let mut sim = Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
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
        let pool = pool();
        let transport = create_transport(&node, &tasks, 1, PORT, Rc::clone(&pool));
        let session = transport.accept().await.unwrap();
        let mut incoming = session.accept().await.unwrap();
        {
            let header = incoming.receiver.recv().await.unwrap().unwrap();
            let protocol = wire::header::decode(&header).unwrap();
            assert_eq!(protocol, (Protocol::Mesh, &[][..]));
        }
        let output = mesh(node, tasks, incoming, pool).await;
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
            Error::Claim(claim::Error::Forged { signer: key(3) }),
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
                // its reply comes after the write of the term. The wait is
                // shorter than each election timeout.
                node.clock().sleep(HALF).await;
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
                node.clock().sleep(HALF).await;
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
            let stop = Error::Stopped(fail_sync(&node));
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
            // Node 1 leads and serves first: the session lives until then.
            peer.node.clock().sleep(seconds(10)).await;
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

// The write of a local proposal waits for a block when the forwarded proposal comes.
#[test]
fn serve_refuses_a_proposal_while_a_write_of_the_log_waits_for_a_block() {
    let (served, seen) = run(
        |node, tasks, incoming| async move {
            let pool = small_pool();
            let (mesh, first) = leader(&node, &tasks, Rc::clone(&pool)).await;
            let held = fill(&pool);
            let mut local = pin!(mesh.propose(home(4)));
            assert!(now(local.as_mut()).await.is_pending());
            node.clock().sleep(TICK).await;
            let served = mesh.serve(public(1), incoming).await;
            drop(held);
            assert_eq!(local.await, Ok(after(first, 1)));
            // The group did not see the change.
            assert_eq!(mesh.propose(home(5)).await, Ok(after(first, 2)));
            served
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    assert_eq!((served, seen), (Err(exhausted(93)), codes(REFUSED)));
}

/// What `serve` gave in 30 s, what a later proposal gave in 30 s, and the position
/// of the first entry.
type Served = (
    Option<Result<(), Error>>,
    Option<Result<Position, Error>>,
    Position,
);

/// Serves `incoming` on `mesh`, then proposes one more change.
async fn serve_then_propose(
    node: &sim::node::Node,
    mesh: &Mesh,
    incoming: Incoming,
    first: Position,
) -> Served {
    let clock = node.clock();
    let serve = pin!(mesh.serve(public(1), incoming));
    let served = within(&clock, seconds(30), serve).await;
    let later = within(&clock, seconds(30), pin!(mesh.propose(home(4)))).await;
    (served, later, first)
}

/// Forwards a proposal, and gives its answer when one comes in 10 s.
async fn forward(peer: Peer) -> Option<Result<Option<Message>, transport::Error>> {
    let (mut sender, mut receiver) = peer.ask(&[propose(3)]).await;
    sender.finish().unwrap();
    let clock = peer.node.clock();
    within(&clock, seconds(10), pin!(next(&mut receiver))).await
}

/// Asserts that `serve` answered the proposal of [`forward`], and that the group took
/// a later one.
fn assert_answered(
    served: Served,
    answer: Option<Result<Option<Message>, transport::Error>>,
) {
    let (served, later, first) = served;
    let at = after(first, 1);
    let answered = Some(Ok(Some(Message::Proposed { at })));
    assert_eq!(
        (served, answer, later),
        (Some(Ok(())), answered, Some(Ok(after(first, 2))))
    );
}

// Another user of the pool leaves room for the one block of a write, 192 bytes. A
// block of the answer that `serve` holds while the group writes takes that room, and
// only the end of the write frees it.
#[test]
fn serve_answers_a_proposal_when_the_pool_has_room_for_only_the_write() {
    let (served, answer) = run(
        |node, tasks, incoming| async move {
            let pool = small_pool();
            let (mesh, first) = leader(&node, &tasks, Rc::clone(&pool)).await;
            let _held = [pool.alloc(3584).unwrap(), pool.alloc(192).unwrap()];
            serve_then_propose(&node, &mesh, incoming, first).await
        },
        forward,
    );
    assert_answered(served, answer);
}

// The mesh and the transport share the pool, as on a shard, so the block of the
// proposal, 128 bytes, comes from the room of the write. The write has its 192 bytes
// only after that block drops.
#[test]
fn serve_drops_the_block_of_the_proposal_before_the_group_writes() {
    let (served, answer) = run_shared(
        0,
        small_pool,
        |node, tasks, incoming, pool| async move {
            let (mesh, first) = leader(&node, &tasks, Rc::clone(&pool)).await;
            let _held = [pool.alloc(3584).unwrap(), pool.alloc(192).unwrap()];
            serve_then_propose(&node, &mesh, incoming, first).await
        },
        forward,
    );
    assert_answered(served, answer);
}

// The system gives no memory for a block of a new size. The log has its block, and
// the answer is the first block of its size.
#[test]
fn serve_gives_no_answer_when_the_pool_has_no_block_for_it() {
    let ((served, taken, later, first), seen) = run(
        |node, tasks, incoming| async move {
            let budget = block::Config { budget: 1 << 20 };
            let (memory, switch) = Scarce::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let (mesh, first) = leader(&node, &tasks, pool).await;
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            switch.refuse();
            let served = mesh.serve(public(1), incoming).await;
            switch.allow();
            let taken = watch.next().await;
            (served, taken, mesh.propose(home(4)).await, first)
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    let error = Error::Pool(block::Error::Refused { requested: 17 });
    assert_eq!(
        error.to_string(),
        "the pool has no block for the mesh now: the system refused memory for a block \
         of 17 bytes"
    );
    assert_eq!(served, Err(error));
    // The group took the change.
    assert_eq!((taken, later), (Ok(Some(key(3))), Ok(after(first, 2))));
    // No code of the mesh.
    assert_eq!(seen, codes(Code(0)));
}

// The group stops in the write of the entry of the proposal. By the draw of the disk,
// the sync that fails keeps the bytes of the entry or not, so the stop is no refusal.
#[test]
fn serve_gives_no_code_of_the_mesh_when_the_group_stops() {
    let mut homes = BTreeSet::new();
    for seed in 0..16 {
        let ((served, home), seen) = run_shared(
            seed,
            create_pool,
            |node, tasks, incoming, _| async move {
                let (mesh, _) = leader(&node, &tasks, create_pool()).await;
                fail_sync(&node);
                let served = mesh.serve(public(1), incoming).await;
                drop(mesh);
                let config = config(&node, &tasks, 1, &[1, 2], &[1]);
                let mesh = Mesh::open(config).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                assert_eq!(watch.next().await, Ok(None));
                (served, watch.next().await)
            },
            |peer| async move { stopped(peer, &[propose(3)]).await },
        );
        let text = "the group stopped: sync of log/log-0 failed with OS error 5";
        assert_eq!(served.unwrap_err().to_string(), text);
        assert_eq!(seen, codes(Code(0)));
        homes.insert(home.unwrap());
    }
    // After a new open, the change applies on each disk that kept its entry.
    let kept = Some(key(3));
    assert!(homes.contains(&kept), "no disk kept the entry: {homes:?}");
    assert!(homes.is_subset(&[Some(key(1)), kept].into()), "{homes:?}");
}

// `serve` does not tell a stop before the proposal from a stop in its write.
#[test]
fn serve_gives_no_code_of_the_mesh_when_the_group_stopped_before_the_proposal() {
    let at = Position {
        term: Term(5),
        index: 1,
    };
    let (served, seen) = run(
        move |node, tasks, incoming| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let data = Data::Bytes(vec![9]);
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![Entry { at, data }],
                commit: 1,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            let cause = Unknown::Kind { kind: 9 };
            let stopped = Stopped::Change { at, cause };
            assert_eq!(watch.next().await, Err(stopped));
            mesh.serve(public(2), incoming).await
        },
        |peer| async move { stopped(peer, &[propose(3)]).await },
    );
    let text = "the group stopped: the committed entry at index 1 of term 5 is not a \
                change: change kind 9 is unknown";
    assert_eq!(served.unwrap_err().to_string(), text);
    assert_eq!(seen, codes(Code(0)));
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
