//! Tests of `Mesh::set_home`: on a cluster of three voters, on one node, and on node
//! 1 with node 2 as a raw peer that plays its leader.

use std::future::pending;

use transport::stream::{Incoming, Receiver, Sender};
use transport::{Class, Code};

use super::send::{self, LIMIT, Peer, create_config, stop};
use super::*;
use crate::bytes::block;

impl Cluster {
    /// Node `node` sets `home` as the home at its next tick.
    fn set(&self, node: u8, home: u8) {
        self.board.lock().unwrap().sets.insert(node, home);
    }

    /// Starts a cluster in which each node proposed itself as the home for 5 s, and
    /// gives the leader, a follower, and the position of the entry of that home.
    fn led(run: u64) -> (Self, u8, u8, Position) {
        let mut cluster = Self::new(run);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let board = cluster.board();
        let (&[leader], &[at]) = (board.led.as_slice(), board.at.as_slice()) else {
            panic!("the group took a proposal from each of {:?}", board.led);
        };
        let follower = IDS.into_iter().find(|&id| id != leader).unwrap();
        (cluster, leader, follower, at)
    }

    /// Sets the chance that a datagram between `node` and each other node is lost.
    fn link_each(&mut self, node: u8, loss: f64) {
        for other in IDS.into_iter().filter(|&id| id != node) {
            self.link(node, other, loss);
        }
    }
}

#[test]
fn a_follower_sets_a_home_through_the_leader() {
    for run in 0..4 {
        let (mut cluster, leader, from, at) = Cluster::led(run);
        cluster.set(from, from);
        cluster.run(seconds(5));
        cluster.script(|_| home(9));
        cluster.run(seconds(5));
        let board = cluster.board();
        assert_eq!(board.set, [(from, Some(key(from)), Ok(()))], "run {run}");
        // The call put one entry in the log of the leader.
        assert_eq!((board.led, board.at), (vec![leader], vec![after(at, 2)]));
        let homes = each(&[Some(key(from)), Some(key(9))]);
        assert_eq!(board.homes, homes, "run {run}");
    }
}

#[test]
fn a_leader_sets_a_home_with_no_stream() {
    let (mut cluster, leader, follower, _) = Cluster::led(1);
    cluster.set(leader, follower);
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_eq!(board.set, [(leader, Some(key(follower)), Ok(()))]);
    assert_eq!(board.homes, each(&[Some(key(follower))]));
}

// The leader puts the entry of the call in its log and loses its lead before a
// quorum has it. The next leader replaces the entry, so the call proposes again.
#[test]
fn a_leader_that_loses_its_lead_sets_the_home_through_the_next_leader() {
    let (mut cluster, old, home, _) = Cluster::led(2);
    cluster.link_each(old, 1.0);
    cluster.set(old, home);
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_eq!((board.set, board.homes), (Vec::new(), Homes::new()));
    cluster.link_each(old, 0.0);
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_eq!(board.set, [(old, Some(key(home)), Ok(()))]);
    assert_eq!(board.homes, each(&[Some(key(home))]));
}

#[test]
fn each_of_three_calls_with_a_different_home_returns() {
    let (mut cluster, ..) = Cluster::led(3);
    for id in IDS {
        cluster.set(id, id);
    }
    cluster.run(seconds(5));
    let board = cluster.board();
    let mut returned: Vec<_> =
        board.set.iter().map(|(id, _, set)| (*id, set)).collect();
    returned.sort_by_key(|&(id, _)| id);
    assert_eq!(returned, IDS.map(|id| (id, &Ok(()))));
    let last = |id| board.homes[&id].last().copied();
    assert!(IDS.map(key).map(Some).map(Some).contains(&last(1)));
    assert_eq!(IDS.map(last), [last(1); 3]);
}

#[test]
fn a_call_on_a_follower_that_is_cut_off_returns_after_the_links_heal() {
    let (mut cluster, _, follower, _) = Cluster::led(4);
    cluster.link_each(follower, 1.0);
    cluster.set(follower, follower);
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_eq!((board.set, board.homes), (Vec::new(), Homes::new()));
    cluster.link_each(follower, 0.0);
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_eq!(board.set, [(follower, Some(key(follower)), Ok(()))]);
    assert_eq!(board.homes, each(&[Some(key(follower))]));
}

#[test]
fn a_member_that_is_not_a_voter_sets_no_home() {
    let mut cluster = Cluster::new(5);
    cluster.board.lock().unwrap().learner = Some(3);
    cluster.script(home);
    cluster.start();
    cluster.run(seconds(5));
    let board = cluster.board();
    let (&[leader], &[at]) = (board.led.as_slice(), board.at.as_slice()) else {
        panic!("the group took a proposal from each of {:?}", board.led);
    };
    cluster.set(3, leader);
    cluster.run(seconds(5));
    cluster.script(|_| home(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_eq!(board.set, [(3, None, Err(Error::NoVote))]);
    // The log of the leader got no entry from the call.
    assert_eq!((board.led, board.at), (vec![leader], vec![after(at, 1)]));
}

#[test]
fn set_home_refuses_a_home_that_is_not_a_member() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let set = now(pin!(mesh.set_home(INDEX, key(9)))).await;
        let error = Error::NotMember(key(9));
        assert_eq!(set, Poll::Ready(Err(error.clone())));
        let text = format!("node {} is not a member of the region", key(9));
        assert_eq!(error.to_string(), text);
    });
}

#[test]
fn set_home_refuses_each_home_on_a_node_that_is_not_a_voter() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
        for (case, home) in [("a member", 2), ("no member", 9)] {
            let set = now(pin!(mesh.set_home(INDEX, key(home)))).await;
            assert_eq!(set, Poll::Ready(Err(Error::NoVote)), "{case}");
        }
        assert_eq!(
            Error::NoVote.to_string(),
            "this node is not a voter, and only a voter proposes a change"
        );
    });
}

// Node 2 is the leader of a joint configuration: node 1 votes only in the half that
// leaves, and node 3 in no half.
#[test]
fn a_voter_of_one_half_of_a_joint_configuration_sets_a_home() {
    for (id, refused) in [(1, false), (3, true)] {
        solo(move |node, tasks| async move {
            let members = [1, 2, 3, 4, 5];
            let mesh = open(&node, &tasks, id, &members, &IDS).await.unwrap();
            let joint = Voters {
                incoming: [key(2), key(4), key(5)].into(),
                outgoing: [key(1), key(2), key(4)].into(),
            };
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![common::change(2, at(1), joint)],
                commit: 0,
            };
            assert_eq!(mesh.receive(public(2), proven(2, id, append)), Ok(()));
            let set = now(pin!(mesh.set_home(INDEX, key(4)))).await;
            let expected = if refused {
                Poll::Ready(Err(Error::NoVote))
            } else {
                Poll::Pending
            };
            assert_eq!(set, expected, "node {id}");
        });
    }
}

#[test]
fn a_lone_voter_sets_a_home_when_it_leads() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let clock = node.clock();
        let start = clock.now();
        assert_eq!(mesh.set_home(INDEX, key(2)).await, Ok(()));
        // The call proposes again at each tick, so it waits for no more than the
        // longest election timeout and one tick.
        assert!(clock.now() - start <= Span::from_nanos(21 * TICK.nanos()));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(2))));
    });
}

#[test]
fn set_home_gives_the_cause_when_the_write_of_its_entry_stops_the_group() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let stopped = Error::Stopped(fail_sync(&node));
        assert_eq!(mesh.set_home(INDEX, key(1)).await, Err(stopped.clone()));
        let again = now(pin!(mesh.set_home(INDEX, key(1)))).await;
        assert_eq!(again, Poll::Ready(Err(stopped.clone())));
        assert_eq!(
            stopped.to_string(),
            "the group stopped: sync of log/log-0 failed with OS error 5"
        );
    });
}

// The pool has no block for the entry of an earlier proposal, so the group takes no
// change.
#[test]
fn set_home_proposes_again_while_the_pool_has_no_block() {
    solo(|node, tasks| async move {
        let pool = small_pool();
        let config = Config {
            pool: Rc::clone(&pool),
            ..config(&node, &tasks, 1, &[1], &[1])
        };
        let mesh = Mesh::start(config).await.unwrap();
        let clock = node.clock();
        lead(&mesh, &clock, home(1)).await;
        let held = fill(&pool);
        let mut waits = pin!(mesh.propose(home(1)));
        assert!(now(waits.as_mut()).await.is_pending());
        clock.sleep(TICK).await;
        assert_eq!(
            started(&mesh, home(1)).await,
            Poll::Ready(Err(exhausted(93)))
        );
        let mut set = pin!(mesh.set_home(INDEX, key(1)));
        for _ in 0..3 {
            assert!(now(set.as_mut()).await.is_pending());
            clock.sleep(TICK).await;
        }
        drop(held);
        assert_eq!(set.await, Ok(()));
    });
}

/// A mesh of node 1 that leads voters 2 and 3, and no voter acknowledges an entry.
async fn create_leader(node: &sim::node::Node, tasks: &Tasks) -> Mesh {
    let mesh = open(node, tasks, 1, &IDS, &IDS).await.unwrap();
    elect(&mesh).await;
    mesh
}

/// Polls `call` until it waits for the outcome of its entry.
async fn wait_for_outcome<F: Future<Output = Result<(), Error>>>(
    mesh: &Mesh,
    clock: &Clock,
    mut call: Pin<&mut F>,
) {
    while mesh.group.borrow().calls.is_empty() {
        assert_eq!(now(call.as_mut()).await, Poll::Pending);
        clock.sleep(Span::MILLISECOND).await;
    }
}

#[test]
fn a_call_that_waits_for_its_entry_gets_the_cause_when_the_group_stops() {
    solo(|node, tasks| async move {
        let mesh = create_leader(&node, &tasks).await;
        let result = Rc::new(RefCell::new(None));
        let (calling, returned) = (mesh.clone(), Rc::clone(&result));
        tasks.spawn(async move {
            let set = calling.set_home(INDEX, key(2)).await;
            *returned.borrow_mut() = Some(set);
        });
        let clock = node.clock();
        clock.sleep(seconds(1)).await;
        assert_eq!(mesh.group.borrow().calls.len(), 1);
        assert_eq!(*result.borrow(), None);
        let stopped = stop(&node, &mesh);
        clock.sleep(Span::MILLISECOND).await;
        assert_eq!(*result.borrow(), Some(Err(Error::Stopped(stopped))));
        assert!(mesh.group.borrow().calls.is_empty());
    });
}

#[test]
fn a_dropped_call_that_waits_for_its_entry_leaves_no_waker_and_no_floor() {
    solo(|node, tasks| async move {
        let mesh = create_leader(&node, &tasks).await;
        let mut call = Box::pin(mesh.set_home(INDEX, key(2)));
        wait_for_outcome(&mesh, &node.clock(), call.as_mut()).await;
        assert_eq!(mesh.group.borrow().calls.len(), 1);
        assert_ne!(mesh.group.borrow().applied, Applied::default());
        drop(call);
        assert!(mesh.group.borrow().calls.is_empty());
        assert_eq!(mesh.group.borrow().applied, Applied::default());
    });
}

/// A position in the term of the tests.
fn at(index: u64) -> Position {
    Position {
        term: common::TERM,
        index,
    }
}

/// The time between two heartbeats of node 2: less than half of the shortest
/// election timeout.
const BEAT: Span = Span::from_nanos(3 * TICK.nanos());

/// Half of the shortest election timeout.
const HALF: Span = Span::from_nanos(5 * TICK.nanos());

/// Node 2 as the leader of node 1 in the term of the tests. It sends a heartbeat
/// each 300 ms.
struct Leader {
    peer: Peer,
    /// The session that node 2 dialed, for its `raft` messages.
    dialed: Session,
    /// The session that node 1 dialed.
    session: Session,
    /// The streams of node 1 that go one way.
    held: Vec<Receiver>,
}

impl Leader {
    async fn new(peer: Peer) -> Self {
        let addresses = [Address::Udp(address(1))];
        let dialed = peer.transport.dial(public(1), &addresses).await.unwrap();
        let mut beats = Self::open(&peer, &dialed).await;
        let (clock, pool) = (peer.node.clock(), Rc::clone(&peer.pool));
        peer.tasks.spawn(async move {
            loop {
                let beat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let beat = block(&pool, &Message::Raft(beat).encode()).unwrap();
                beats.send(beat).await.unwrap();
                clock.sleep(BEAT).await;
            }
        });
        // Node 1 dials for its reply to the first heartbeat.
        let session = peer.session().await;
        Self {
            peer,
            dialed,
            session,
            held: Vec::new(),
        }
    }

    /// A stream to node 1 that goes one way, after its header.
    async fn open(peer: &Peer, dialed: &Session) -> Sender {
        let mut sender = dialed.open_sender(Class::Command).await.unwrap();
        let header = peer.block(&wire::header::encode(Protocol::Mesh));
        sender.send(header).await.unwrap();
        sender
    }

    fn clock(&self) -> Clock {
        self.peer.node.clock()
    }

    /// The next proposal of node 1, with the stream of its answer. Node 1 ended its
    /// half of the stream.
    async fn proposal(&mut self) -> (Change, Sender) {
        loop {
            let Incoming {
                class,
                mut receiver,
                sender,
            } = self.session.accept().await.unwrap();
            assert_eq!(class, Class::Command);
            let header = receiver.recv().await.unwrap().unwrap();
            let protocol = wire::header::decode(&header).unwrap();
            assert_eq!(protocol, (Protocol::Mesh, &[][..]));
            let Some(sender) = sender else {
                self.held.push(receiver);
                continue;
            };
            let bytes = receiver.recv().await.unwrap().unwrap();
            let Some(Message::Propose { change }) = Message::decode(&bytes) else {
                panic!("node 1 asked with no proposal");
            };
            let end = receiver.recv().await.unwrap();
            assert!(end.is_none(), "node 1 sent more than one proposal");
            return (change, sender);
        }
    }

    /// The next proposal of node 1, or `None` when none comes in `span`.
    async fn proposal_within(&mut self, span: Span) -> Option<(Change, Sender)> {
        let clock = self.clock();
        within(&clock, span, self.proposal()).await
    }

    /// Sends node 1 the entry of `change` at `at`, as committed.
    async fn append(&self, change: &Change, at: Position) {
        let entry = Entry {
            at,
            data: Data::Bytes(encoded(change)),
        };
        let append = Body::Append {
            prev: Position::default(),
            entries: vec![entry],
            commit: at.index,
        };
        let append = Message::Raft(proven(2, 1, append)).encode();
        let mut sender = Self::open(&self.peer, &self.dialed).await;
        sender.send(self.peer.block(&append)).await.unwrap();
        sender.finish().unwrap();
    }

    /// Answers a proposal with the position `at`.
    async fn answer(
        &self,
        sender: &mut Sender,
        at: Position,
    ) -> Result<(), transport::Error> {
        let answer = Message::Proposed { at }.encode();
        sender.send(self.peer.block(&answer)).await?;
        sender.finish()
    }

    async fn rest(&self, span: Span) {
        self.clock().sleep(span).await;
    }
}

/// Runs `call` on node 1 with its mesh, which serves each stream of node 2, and
/// `peer` on node 2 as the leader, for 30 s. Gives what each returned.
fn run<M, P>(
    call: impl FnOnce(sim::node::Node, Mesh) -> M + Send + 'static,
    peer: impl FnOnce(Leader) -> P + Send + 'static,
) -> (M::Output, P::Output)
where
    M: Future<Output: Send + 'static> + 'static,
    P: Future<Output: Send + 'static> + 'static,
{
    let called = Arc::new(Mutex::new(None));
    let result = Arc::clone(&called);
    let mesh = move |node: sim::node::Node, tasks: Tasks| async move {
        let config = create_config(&node, &tasks, create_pool());
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let (serving, streams) = (mesh.clone(), tasks.clone());
        tasks.spawn(async move {
            accept(serving, transport, streams).await;
        });
        let output = call(node, mesh).await;
        *result.lock().unwrap() = Some(output);
        pending::<()>().await;
    };
    let sent = send::run(LIMIT, mesh, |side| async move {
        peer(Leader::new(side).await).await
    });
    let output = called.lock().unwrap().take();
    (output.expect("the call did not return in 30 s"), sent)
}

/// Sets node 1 as the home on `mesh`, and gives the result with the home after.
async fn set(mesh: Mesh) -> (Result<(), Error>, Option<node::Key>) {
    let set = mesh.set_home(INDEX, key(1)).await;
    (set, mesh.group.borrow().state.home(INDEX))
}

#[test]
fn a_proposal_with_no_answer_goes_again_on_a_new_stream_after_one_election_timeout() {
    let call = |_, mesh| set(mesh);
    let (set, (changes, gap, late)) = run(call, |mut leader| async move {
        let clock = leader.clock();
        let (first, mut old) = leader.proposal().await;
        let start = clock.now();
        let (second, mut sender) = leader.proposal().await;
        let gap = clock.now() - start;
        let late = leader.answer(&mut old, at(1)).await;
        leader.answer(&mut sender, at(1)).await.unwrap();
        leader.append(&home(1), at(1)).await;
        leader.rest(seconds(2)).await;
        ([first, second], gap, late)
    });
    assert_eq!(set, (Ok(()), Some(key(1))));
    assert_eq!(changes, [home(1), home(1)]);
    // The answer has one election timeout, and the next try starts one tick later.
    let ms = gap.nanos() / Span::MILLISECOND.nanos();
    assert!(
        (1100..1200).contains(&ms),
        "{ms} ms between the two proposals"
    );
    assert_eq!(late, Err(transport::Error::Stopped { code: Code(0) }));
}

#[test]
fn an_answer_that_comes_after_its_entry_applied_ends_the_call() {
    let call = |node: sim::node::Node, mesh: Mesh| async move {
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        let mut call = pin!(set(mesh.clone()));
        let applied = {
            let mut next = pin!(watch.next());
            let applied = poll_fn(|cx| {
                let Poll::Pending = call.as_mut().poll(cx) else {
                    panic!("the call returned before the answer");
                };
                next.as_mut().poll(cx)
            });
            applied.await
        };
        let applied = (applied, node.clock().now());
        let returned = call.await;
        (applied, returned, node.clock().now())
    };
    let ((applied, returned, end), ()) = run(call, |mut leader| async move {
        let (_, mut sender) = leader.proposal().await;
        leader.append(&home(1), at(1)).await;
        leader.rest(HALF).await;
        leader.answer(&mut sender, at(1)).await.unwrap();
        leader.rest(seconds(2)).await;
    });
    let (home, start) = applied;
    assert_eq!(home, Ok(Some(key(1))));
    assert_eq!(returned, (Ok(()), Some(key(1))));
    let ms = (end - start).nanos() / Span::MILLISECOND.nanos();
    assert!(
        (400..600).contains(&ms),
        "the call returned {ms} ms after the entry"
    );
}

#[test]
fn a_dropped_call_that_waits_for_the_answer_stops_its_stream() {
    let asked = Arc::new(Mutex::new(false));
    let got = Arc::clone(&asked);
    let call = move |node: sim::node::Node, mesh: Mesh| async move {
        let clock = node.clock();
        let mut call = pin!(mesh.set_home(INDEX, key(1)));
        while !*asked.lock().unwrap() {
            assert_eq!(now(call.as_mut()).await, Poll::Pending);
            clock.sleep(Span::MILLISECOND).await;
        }
    };
    let ((), (late, more)) = run(call, |mut leader| async move {
        let (_, mut sender) = leader.proposal().await;
        *got.lock().unwrap() = true;
        leader.rest(HALF).await;
        let late = leader.answer(&mut sender, at(1)).await;
        let more = leader.proposal_within(seconds(3)).await;
        (late, more.map(|(change, _)| change))
    });
    assert_eq!(late, Err(transport::Error::Stopped { code: Code(0) }));
    assert_eq!(more, None);
}
