//! Tests of `Mesh::set_home`: on a cluster of three voters, on one node, and on node
//! 1 with node 2 as a raw peer that plays its leader.

use std::future::pending;

use transport::Class;
use transport::stream::{Incoming, Receiver, Sender};

use super::send::{self, LIMIT, Peer, create_config, stop};
use super::*;

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

// The check reads the voters of the log, which change when the node appends the
// entry, before the commit.
#[test]
fn a_promoted_node_sets_a_home_from_the_append_of_its_promotion() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
        let set = now(pin!(mesh.set_home(INDEX, key(2)))).await;
        assert_eq!(set, Poll::Ready(Err(Error::NoVote)));
        let promoted = Voters {
            incoming: IDS.map(key).into(),
            outgoing: [].into(),
        };
        let append = Body::Append {
            prev: Position::default(),
            entries: vec![common::change(2, at(1), promoted)],
            commit: 0,
        };
        assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
        let set = now(pin!(mesh.set_home(INDEX, key(2)))).await;
        assert_eq!(set, Poll::Pending);
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
        let clock = node.clock();
        let start = clock.now();
        assert_eq!(mesh.set_home(INDEX, key(1)).await, Err(stopped.clone()));
        // The call waits for no next try.
        assert!(clock.now() - start < TICK);
        let again = now(pin!(mesh.set_home(INDEX, key(1)))).await;
        assert_eq!(again, Poll::Ready(Err(stopped.clone())));
        assert_eq!(
            stopped.to_string(),
            "the group stopped: sync of log/log-0 failed with OS error 5"
        );
    });
}

#[test]
fn set_home_gives_the_cause_of_a_stop_before_each_other_refusal() {
    let cases = [("no voter", [2, 3].as_slice(), 2), ("no member", &IDS, 9)];
    for (case, voters, home) in cases {
        solo(move |node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, voters).await.unwrap();
            let stopped = stop(&node, &mesh);
            node.clock().sleep(Span::MILLISECOND).await;
            let set = now(pin!(mesh.set_home(INDEX, key(home)))).await;
            assert_eq!(set, Poll::Ready(Err(Error::Stopped(stopped))), "{case}");
        });
    }
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

// One reply of node 2 commits the entry of the call and, after it, an entry that is
// not a change.
#[test]
fn a_call_gives_ok_when_its_entry_applies_in_the_batch_that_stops_the_group() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
        let first = elect(&mesh).await;
        let mut call = pin!(mesh.set_home(INDEX, key(2)));
        wait_for_outcome(&mesh, &node.clock(), call.as_mut()).await;
        let bad = mesh.propose_data(vec![9]).await.unwrap();
        assert_eq!(bad, after(first, 2));
        let reply = raft::Message {
            term: first.term,
            ..message(2, 1, Body::AppendReply { last: bad.index })
        };
        assert_eq!(mesh.receive(public(2), reply), Ok(()));
        assert_eq!(call.await, Ok(()));
        let cause = Unknown::Kind { kind: 9 };
        let stopped = Stopped::Change { at: bad, cause };
        assert_eq!(mesh.watch(INDEX).next().await, Err(stopped));
    });
}

// A waker or a floor that stays only takes memory, which no call shows, so this
// test reads the group.
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
    /// Whether node 2 sends no heartbeat now.
    silent: Rc<Cell<bool>>,
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
        let silent = Rc::new(Cell::new(false));
        let ended = Rc::clone(&silent);
        peer.tasks.spawn(async move {
            loop {
                if !ended.get() {
                    let beat = proven(2, 1, Body::Heartbeat { commit: 0 });
                    let beat = block(&pool, &Message::Raft(beat).encode()).unwrap();
                    beats.send(beat).await.unwrap();
                }
                clock.sleep(BEAT).await;
            }
        });
        // Node 1 dials for its reply to the first heartbeat.
        let session = peer.session().await;
        Self {
            peer,
            silent,
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

    /// The next proposal of node 1, with the stream of its answer.
    async fn proposal(&mut self) -> (Change, Asked) {
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
            return (change, Asked { receiver, sender });
        }
    }

    /// The next proposal of node 1, or `None` when none comes in `span`.
    async fn proposal_within(&mut self, span: Span) -> Option<(Change, Asked)> {
        let clock = self.clock();
        within(&clock, span, self.proposal()).await
    }

    /// Sends node 1 the entry of `change` at `at`, as committed.
    async fn append(&self, change: &Change, at: Position) {
        self.append_data(encoded(change), at).await;
    }

    /// Sends node 1 an entry of `data` at `at`, as committed.
    async fn append_data(&self, data: Vec<u8>, at: Position) {
        let entry = Entry {
            at,
            data: Data::Bytes(data),
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
        asked: &mut Asked,
        at: Position,
    ) -> Result<(), transport::Error> {
        let answer = Message::Proposed { at }.encode();
        asked.sender.send(self.peer.block(&answer)).await?;
        asked.sender.finish()
    }

    async fn rest(&self, span: Span) {
        self.clock().sleep(span).await;
    }

    /// The entry of `change` at `at`, as committed, in a message of `term`.
    fn append_in(&self, term: Term, change: &Change, at: Position) -> block::Block {
        let entry = Entry {
            at,
            data: Data::Bytes(encoded(change)),
        };
        let append = Body::Append {
            prev: Position::default(),
            entries: vec![entry],
            commit: at.index,
        };
        let append = common::proven_in(term, 2, 1, append);
        self.peer.block(&Message::Raft(append).encode())
    }
}

/// The stream of a proposal of node 1, after the proposal.
struct Asked {
    receiver: Receiver,
    sender: Sender,
}

impl Asked {
    /// How node 1 ends its half: `Ok` when it ended the stream with no more message.
    async fn end(&mut self) -> Result<(), transport::Error> {
        let end = self.receiver.recv().await?;
        assert!(end.is_none(), "node 1 sent more than one proposal");
        Ok(())
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
    (set, mesh.watch(INDEX).next().await.unwrap())
}

// Node 2 holds its answer for 3 s and leads, so the try waits. It then sends no
// heartbeat, and node 1 has no leader after its election timeout.
#[test]
fn a_proposal_with_no_answer_goes_again_only_after_node_1_has_no_leader() {
    let call = |node: sim::node::Node, mesh| async move {
        let clock = node.clock();
        let mut set = pin!(set(mesh));
        let (mut last, mut quiet) = (clock.now(), 0);
        let set = poll_fn(|cx| {
            let now = clock.now();
            quiet = quiet.max((now - last).nanos());
            last = now;
            set.as_mut().poll(cx)
        });
        (set.await, quiet)
    };
    let leader = |mut leader: Leader| async move {
        let clock = leader.clock();
        let (first, mut old) = leader.proposal().await;
        let early = leader.proposal_within(seconds(3)).await;
        leader.silent.set(true);
        let start = clock.now();
        let end = old.end().await;
        let gap = clock.now() - start;
        let late = leader.answer(&mut old, at(1)).await;
        leader.silent.set(false);
        let (second, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(1)).await.unwrap();
        leader.append(&home(1), at(1)).await;
        leader.rest(seconds(2)).await;
        let early = early.map(|(change, _)| change);
        ([first, second], early, gap, late, [end, asked.end().await])
    };
    let ((set, quiet), (changes, early, gap, late, ends)) = run(call, leader);
    assert_eq!(set, (Ok(()), Some(key(1))));
    assert_eq!(changes, [home(1), home(1)]);
    assert_eq!(early, None);
    // The last heartbeat came at most 300 ms before, and an election timeout is 1 s
    // to 2 s.
    let ms = gap.nanos() / Span::MILLISECOND.nanos();
    assert!(
        (700..2100).contains(&ms),
        "the try ended {ms} ms after the last heartbeat"
    );
    // No heartbeat and no tick wakes the call while node 2 leads.
    let ms = quiet / Span::MILLISECOND.nanos();
    assert!(
        ms >= 3700,
        "the longest time with no poll of the call was {ms} ms"
    );
    assert_eq!(late, Err(transport::Error::Stopped { code: Code(0) }));
    // The try that gave up took its proposal back, and the other ended its stream.
    let reset = transport::Error::Reset { code: Code(0) };
    assert_eq!(ends, [Err(reset), Ok(())]);
}

// Node 2 stays the leader and sends one heartbeat of the next term, then none. Node 1
// got the last heartbeat of term 5 at most 600 ms before, and an election timeout is
// 10 ticks or more: in the first 300 ms, only the new term ends the try.
#[test]
fn a_try_ends_when_the_same_leader_leads_a_later_term() {
    let call = |node: sim::node::Node, mesh: Mesh| async move {
        within(&node.clock(), seconds(10), set(mesh)).await
    };
    let (set, (end, gap)) = run(call, |mut leader| async move {
        let clock = leader.clock();
        let (_, mut old) = leader.proposal().await;
        leader.silent.set(true);
        leader.rest(BEAT).await;
        let beat = common::proven_in(Term(6), 2, 1, Body::Heartbeat { commit: 0 });
        let mut sender = Leader::open(&leader.peer, &leader.dialed).await;
        let start = clock.now();
        let beat = leader.peer.block(&Message::Raft(beat).encode());
        sender.send(beat).await.unwrap();
        sender.finish().unwrap();
        let end = old.end().await;
        (end, clock.now() - start)
    });
    assert_eq!(set, None);
    assert_eq!(end, Err(transport::Error::Reset { code: Code(0) }));
    let ms = gap.nanos() / Span::MILLISECOND.nanos();
    assert!(
        ms < 300,
        "the try ended {ms} ms after the heartbeat of term 6"
    );
}

// The answer comes while nothing polls the call. Node 1 then takes a heartbeat of
// term 6, which the read of its term shows, so the next poll of the call sees the
// answer and the new term.
#[test]
fn one_poll_that_sees_the_answer_and_a_later_term_keeps_the_answer() {
    let asked = Arc::new(Mutex::new(false));
    let got = Arc::clone(&asked);
    let call = move |node: sim::node::Node, mesh: Mesh| async move {
        let clock = node.clock();
        let mut call = pin!(set(mesh.clone()));
        while !*asked.lock().unwrap() {
            assert!(now(call.as_mut()).await.is_pending());
            clock.sleep(Span::MILLISECOND).await;
        }
        clock.sleep(HALF).await;
        let beat = common::proven_in(Term(6), 2, 1, Body::Heartbeat { commit: 0 });
        assert_eq!(mesh.receive(public(2), beat), Ok(()));
        assert_eq!(term(&mesh), Term(6));
        within(&clock, seconds(10), call).await
    };
    let (set, (more, end)) = run(call, |mut leader| async move {
        let (_, mut asked) = leader.proposal().await;
        leader.silent.set(true);
        *got.lock().unwrap() = true;
        leader.answer(&mut asked, at(1)).await.unwrap();
        leader.rest(seconds(1)).await;
        let mut sender = Leader::open(&leader.peer, &leader.dialed).await;
        let append = leader.append_in(Term(6), &home(1), at(1));
        sender.send(append).await.unwrap();
        let more = leader.proposal_within(seconds(3)).await;
        (more.map(|(change, _)| change), asked.end().await)
    });
    assert_eq!(more, None);
    assert_eq!(end, Ok(()));
    assert_eq!(set, Some((Ok(()), Some(key(1)))));
}

/// The next value of `watch`, which comes while `call` waits.
async fn next_before<F: Future>(
    watch: &mut Watch,
    mut call: Pin<&mut F>,
) -> Result<Option<node::Key>, Stopped> {
    let mut next = pin!(watch.next());
    poll_fn(|cx| {
        let Poll::Pending = call.as_mut().poll(cx) else {
            panic!("the call returned before the watch gave a value");
        };
        next.as_mut().poll(cx)
    })
    .await
}

#[test]
fn an_answer_that_comes_after_its_entry_applied_ends_the_call() {
    let call = |node: sim::node::Node, mesh: Mesh| async move {
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        let mut call = pin!(set(mesh.clone()));
        let applied = next_before(&mut watch, call.as_mut()).await;
        let applied = (applied, node.clock().now());
        let returned = call.await;
        (applied, returned, node.clock().now())
    };
    let ((applied, returned, end), ()) = run(call, |mut leader| async move {
        let (_, mut asked) = leader.proposal().await;
        leader.append(&home(1), at(1)).await;
        leader.rest(HALF).await;
        leader.answer(&mut asked, at(1)).await.unwrap();
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
    let ((), (late, end, more)) = run(call, |mut leader| async move {
        let (_, mut asked) = leader.proposal().await;
        *got.lock().unwrap() = true;
        leader.rest(HALF).await;
        let late = leader.answer(&mut asked, at(1)).await;
        let end = asked.end().await;
        let more = leader.proposal_within(seconds(3)).await;
        (late, end, more.map(|(change, _)| change))
    });
    assert_eq!(late, Err(transport::Error::Stopped { code: Code(0) }));
    assert_eq!(end, Err(transport::Error::Reset { code: Code(0) }));
    assert_eq!(more, None);
}

// Node 2 sends no heartbeat after the stop: `accept` takes no stream that the group
// refuses.
#[test]
fn a_call_that_waits_for_the_answer_gets_the_cause_when_the_group_stops() {
    let call = |_, mesh: Mesh| async move {
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        let mut call = pin!(mesh.set_home(INDEX, key(1)));
        let mut next = pin!(watch.next());
        poll_fn(
            |cx| match (next.as_mut().poll(cx), call.as_mut().poll(cx)) {
                (Poll::Pending, Poll::Pending) => Poll::Pending,
                (Poll::Ready(stopped), Poll::Ready(set)) => Poll::Ready((stopped, set)),
                (stopped, set) => panic!("only one returned: {stopped:?}, {set:?}"),
            },
        )
        .await
    };
    let ((stopped, set), end) = run(call, |mut leader| async move {
        let (_, mut asked) = leader.proposal().await;
        leader.silent.set(true);
        leader.rest(BEAT).await;
        leader.append_data(vec![9], at(1)).await;
        asked.end().await
    });
    let cause = Unknown::Kind { kind: 9 };
    let cause = Stopped::Change { at: at(1), cause };
    assert_eq!(stopped, Err(cause.clone()));
    assert_eq!(set, Err(Error::Stopped(cause)));
    // The group dropped its sessions at the stop, so the try held the last handle
    // of this one.
    assert_eq!(end, Err(transport::Error::PeerClosed { code: Code(0) }));
}

/// How node 2 refuses a proposal.
enum Refusal {
    /// It answers with these bytes, and ends its half.
    Answer(Vec<u8>),
    /// It ends its half with no answer.
    End,
    /// It resets its half and stops the other one, as `serve` does.
    Reset,
}

#[test]
fn a_proposal_that_the_leader_refuses_goes_again_after_one_tick() {
    let follower = |leader| Message::NotLeader { leader }.encode();
    let beat = proven(2, 1, Body::Heartbeat { commit: 0 });
    let reset = transport::Error::Reset { code: Code(0) };
    let cases = [
        ("no leader", Refusal::Answer(follower(None)), Ok(())),
        ("a leader", Refusal::Answer(follower(Some(key(3)))), Ok(())),
        (
            "a `raft` message",
            Refusal::Answer(Message::Raft(beat).encode()),
            Ok(()),
        ),
        (
            "a proposal",
            Refusal::Answer(Message::Propose { change: home(1) }.encode()),
            Ok(()),
        ),
        ("no message", Refusal::Answer(vec![0xff]), Ok(())),
        ("an end", Refusal::End, Err(reset.clone())),
        ("a reset", Refusal::Reset, Err(reset)),
    ];
    for (case, refusal, ended) in cases {
        let call = |_, mesh| set(mesh);
        let (set, (changes, gap, end)) = run(call, |mut leader| async move {
            let clock = leader.clock();
            let (first, mut refused) = leader.proposal().await;
            if let Refusal::Answer(bytes) = &refusal {
                let answer = leader.peer.block(bytes);
                refused.sender.send(answer).await.unwrap();
            }
            if !matches!(refusal, Refusal::Reset) {
                refused.sender.finish().unwrap();
            }
            let start = clock.now();
            let end = match refusal {
                Refusal::Answer(_) | Refusal::End => refused.end().await,
                Refusal::Reset => {
                    let Asked {
                        mut receiver,
                        sender,
                    } = refused;
                    sender.reset(Code(16));
                    let end = receiver.recv().await.map(drop);
                    receiver.stop(Code(16));
                    end
                }
            };
            let (second, mut asked) = leader.proposal().await;
            let gap = clock.now() - start;
            leader.answer(&mut asked, at(1)).await.unwrap();
            leader.append(&home(1), at(1)).await;
            leader.rest(seconds(2)).await;
            ([first, second], gap, end)
        });
        assert_eq!(set, (Ok(()), Some(key(1))), "{case}");
        // Node 1 ends its stream after each answer, also one that refuses, and resets
        // it when the leader ends or resets with no answer.
        assert_eq!(end, ended, "{case}");
        assert_eq!(changes, [home(1), home(1)], "{case}");
        let ms = gap.nanos() / Span::MILLISECOND.nanos();
        assert!(
            (100..200).contains(&ms),
            "{case}: {ms} ms between the two proposals"
        );
    }
}

#[test]
fn an_answer_stands_when_the_leader_stopped_its_half() {
    let call = |_, mesh| set(mesh);
    let (set, more) = run(call, |mut leader| async move {
        let (
            _,
            Asked {
                receiver,
                mut sender,
            },
        ) = leader.proposal().await;
        receiver.stop(Code(16));
        leader.rest(HALF).await;
        let answer = Message::Proposed { at: at(1) }.encode();
        sender.send(leader.peer.block(&answer)).await.unwrap();
        sender.finish().unwrap();
        leader.append(&home(1), at(1)).await;
        let more = leader.proposal_within(seconds(3)).await;
        more.map(|(change, _)| change)
    });
    assert_eq!(more, None);
    assert_eq!(set, (Ok(()), Some(key(1))));
}

/// Waits until the group of `mesh` holds a session to node 2. No public call shows
/// the session, and with none a try ends before it takes a block, so this reads the
/// group.
async fn wait_for_session(mesh: &Mesh, clock: &Clock) {
    while !mesh.group.borrow().sessions.contains_key(&key(2)) {
        clock.sleep(Span::MILLISECOND).await;
    }
}

#[test]
fn no_proposal_goes_to_the_leader_while_the_pool_has_no_block() {
    let call = |node: sim::node::Node, mesh: Mesh| async move {
        let clock = node.clock();
        wait_for_session(&mesh, &clock).await;
        let held = fill(&mesh.pool);
        let mut call = pin!(set(mesh.clone()));
        let end = clock.now() + seconds(2);
        while clock.now() < end {
            assert_eq!(now(call.as_mut()).await, Poll::Pending);
            clock.sleep(Span::MILLISECOND).await;
        }
        drop(held);
        call.await
    };
    let (set, (early, change)) = run(call, |mut leader| async move {
        let early = leader.proposal_within(seconds(1)).await;
        let (change, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(1)).await.unwrap();
        leader.append(&home(1), at(1)).await;
        leader.rest(seconds(2)).await;
        (early.map(|(change, _)| change), change)
    });
    assert_eq!(set, (Ok(()), Some(key(1))));
    assert_eq!((early, change), (None, home(1)));
}

#[test]
fn a_proposal_goes_on_the_next_session_when_the_session_to_the_leader_closed() {
    let call = |_, mesh| set(mesh);
    let (set, change) = run(call, |mut leader| async move {
        leader.session.close(Code(7));
        leader.session = leader.peer.session().await;
        let (change, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(1)).await.unwrap();
        leader.append(&home(1), at(1)).await;
        leader.rest(seconds(2)).await;
        change
    });
    assert_eq!((set, change), ((Ok(()), Some(key(1))), home(1)));
}

/// An index that only a probe for the leader sets.
const PROBE: channel::Key = channel::Key::from_u128(8);

/// The time between two looks at the board.
const STEP: Span = Span::from_nanos(250 * Span::MILLISECOND.nanos());

impl Cluster {
    /// Runs in steps of 250 ms until `done`.
    ///
    /// # Panics
    ///
    /// When `done` does not hold after `steps` steps.
    fn run_until(&mut self, steps: u32, done: impl Fn(&Board) -> bool) {
        for _ in 0..steps {
            if done(&self.board.lock().unwrap()) {
                return;
            }
            self.run(STEP);
        }
        let done = done(&self.board.lock().unwrap());
        assert!(done, "not done after {steps} steps");
    }

    /// Each node that takes a proposal in the next second: the leader.
    fn leaders(&mut self) -> Vec<u8> {
        self.board.lock().unwrap().led.clear();
        self.script(|id| Change::Home {
            index: PROBE,
            home: key(id),
        });
        self.run(seconds(1));
        let mut board = self.board.lock().unwrap();
        board.script.clear();
        mem::take(&mut board.led)
    }
}

/// The home of `INDEX` that the watch of node `id` gave last.
fn last(board: &Board, id: u8) -> Option<node::Key> {
    let homes = board.homes.get(&id);
    homes.and_then(|homes| homes.last().copied()).flatten()
}

// The first try waits on the session to the old leader, which is cut off, and gives
// up. A next leader takes a later try, and then a second call. The old leader then
// leads again, and its link to the caller heals with the session still open.
#[test]
fn a_try_that_gave_up_sets_no_home_after_a_later_call_returned() {
    for run in [0, 1] {
        let (mut cluster, old, caller, _) = Cluster::led(run);
        let third = IDS.into_iter().find(|id| ![old, caller].contains(id));
        let third = third.unwrap();
        cluster.link_each(old, 1.0);
        cluster.set(caller, caller);
        cluster.run_until(32, |board| board.set.len() == 1);
        cluster.set(caller, third);
        cluster.run_until(20, |board| board.set.len() == 2);
        let set = [caller, third].map(|home| (caller, Some(key(home)), Ok(())));
        assert_eq!(cluster.board.lock().unwrap().set, set, "run {run}");
        // The old leader gets the log from the third node, with no word of the caller.
        cluster.link(caller, third, 1.0);
        cluster.link(old, third, 0.0);
        cluster.run_until(32, |board| last(board, old) == Some(key(third)));
        // The two hold the same log, so one of them leads after each cut between them.
        let mut led = cluster.leaders();
        for _ in 0..4 {
            if led == [old] {
                break;
            }
            cluster.link(old, third, 1.0);
            cluster.run(seconds(3));
            cluster.link(old, third, 0.0);
            cluster.run(seconds(3));
            led = cluster.leaders();
        }
        assert_eq!(led, [old], "run {run}");
        cluster.link_each(caller, 0.0);
        cluster.run(seconds(15));
        let board = cluster.board();
        assert_eq!(board.set.len(), 2, "run {run}");
        let homes = IDS.map(|id| last(&board, id));
        assert_eq!(homes, [Some(key(third)); 3], "run {run}");
    }
}

impl Cluster {
    /// Sets the delay of each datagram between `node` and each other node.
    fn delay_each(&mut self, node: u8, delay: Span) {
        let config = link::Config {
            delay,
            ..link::Config::default()
        };
        for other in IDS.into_iter().filter(|&id| id != node) {
            let (a, b) = (self.node(node).clone(), self.node(other).clone());
            self.sim.link(&a, &b, config);
            self.sim.link(&b, &a, config);
        }
    }

    /// Starts a call on a follower whose datagrams take 600 ms each way, and runs for
    /// `wait` after. Gives the leader, the follower, and the position of the last
    /// entry before the call.
    fn asked(run: u64, wait: Span) -> (Self, u8, u8, Position) {
        let (mut cluster, leader, follower, at) = Self::led(run);
        cluster.delay_each(follower, ticks(6));
        cluster.run(seconds(5));
        cluster.set(follower, follower);
        cluster.run(wait);
        (cluster, leader, follower, at)
    }
}

fn ticks(count: i64) -> Span {
    Span::from_nanos(TICK.nanos().checked_mul(count).unwrap())
}

// The answer comes 1.2 s after the proposal, which is more than the shortest election
// timeout. The leader leads all the time, so the call waits for the answer.
#[test]
fn a_call_puts_one_entry_in_the_log_when_the_answer_of_the_leader_is_slow() {
    let (mut cluster, leader, follower, at) = Cluster::asked(0, seconds(30));
    let board = cluster.board();
    assert_eq!(board.set, [(follower, Some(key(follower)), Ok(()))]);
    cluster.script(|_| home(9));
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_eq!((board.led, board.at), (vec![leader], vec![after(at, 2)]));
}

// The leader crashes 300 ms after the call, before it gets the proposal, or 900 ms
// after, when its answer is in flight. Each datagram takes the default time from the
// crash, so that the two nodes that run elect a leader.
#[test]
fn a_call_returns_through_the_next_leader_when_the_leader_crashes() {
    for (run, wait) in [(0, 3), (1, 3), (0, 9), (1, 9)] {
        let (mut cluster, leader, follower, at) = Cluster::asked(run, ticks(wait));
        let node = cluster.node(leader).clone();
        cluster.sim.crash(&node, Crash::Power);
        cluster.link_each(follower, 0.0);
        cluster.run(seconds(20));
        let board = cluster.board();
        let set = [(follower, Some(key(follower)), Ok(()))];
        assert_eq!(board.set, set, "run {run}, {wait} ticks");
        cluster.script(|_| home(9));
        cluster.run(seconds(10));
        let board = cluster.board();
        let next: Vec<u8> = IDS.into_iter().filter(|&id| id != leader).collect();
        assert!(matches!(board.led[..], [led] if next.contains(&led)));
        // The leader of the next term appends an entry of its own, and the call
        // appends one.
        let indexes: Vec<u64> = board.at.iter().map(|at| at.index).collect();
        assert_eq!(indexes, [after(at, 3).index], "run {run}, {wait} ticks");
    }
}

// The leader gets the proposal, and the cut drops its answer. The first try ends when
// the follower hears no leader, and the call proposes again after the heal.
#[test]
fn a_call_that_is_cut_off_while_it_waits_for_the_answer_returns_after_the_heal() {
    for run in 0..4 {
        let (mut cluster, leader, follower, at) = Cluster::asked(run, ticks(3));
        cluster.link_each(follower, 1.0);
        cluster.run(seconds(5));
        let board = cluster.board();
        assert_eq!(board.set, [], "run {run}");
        assert_eq!(board.homes[&leader], [Some(key(follower))], "run {run}");
        cluster.link_each(follower, 0.0);
        cluster.run(seconds(10));
        let board = cluster.board();
        assert_eq!(
            board.set,
            [(follower, Some(key(follower)), Ok(()))],
            "run {run}"
        );
        cluster.script(|_| home(9));
        cluster.run(seconds(5));
        let board = cluster.board();
        let took = (board.led, board.at);
        assert_eq!(took, (vec![leader], vec![after(at, 3)]), "run {run}");
    }
}
