//! Election scenarios ported from the tests of etcd/raft (Copyright 2015 The etcd
//! Authors, Apache License 2.0, see `LICENSE`). This file is modified from the etcd
//! source: `README.md` lists each source and the changes. `replication.rs` holds
//! the replication scenarios on the same network.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "replication.rs"]
mod replication;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{Body, Entry, Hard, Message, Position, Raft, Role, Term};

use replication::{Disk, start};
use types::node;

const ELECTION: u32 = 10;

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn at_term(term: u64) -> Hard {
    Hard {
        term: Term(term),
        vote: None,
    }
}

/// Node `id` with a log of `last.index` entries, all in `last.term`, and its disk.
fn build(
    id: u8,
    voters: &[u8],
    election: u32,
    hard: Hard,
    last: Position,
) -> (Raft, Disk) {
    let entries = (1..=last.index)
        .map(|index| Entry {
            at: Position {
                term: last.term,
                index,
            },
            data: Vec::new(),
        })
        .collect();
    start(id, voters, election, hard, entries, 0)
}

fn drain(raft: &mut Raft) -> Vec<Message> {
    raft.ready().messages
}

/// The etcd test network: it delivers messages in order until none remain. A voter
/// with no peer never answers.
struct Network {
    peers: BTreeMap<node::Key, Raft>,
    disks: BTreeMap<node::Key, Disk>,
    cuts: BTreeSet<(node::Key, node::Key)>,
}

impl Network {
    fn new(peers: impl IntoIterator<Item = (Raft, Disk)>) -> Self {
        let (peers, disks) = peers
            .into_iter()
            .map(|(peer, disk)| {
                let key = peer.key();
                ((key, peer), (key, disk))
            })
            .unzip();
        Self {
            peers,
            disks,
            cuts: BTreeSet::new(),
        }
    }

    /// `size` voters, all with the stored state `hard`. Only `present` have a peer.
    fn of(size: u8, present: &[u8], hard: Hard) -> Self {
        let voters: Vec<u8> = (1..=size).collect();
        Self::new(
            present
                .iter()
                .map(|&id| build(id, &voters, ELECTION, hard, Position::default())),
        )
    }

    fn peer(&self, id: u8) -> &Raft {
        &self.peers[&key(id)]
    }

    /// Makes each node campaign, then delivers until quiet.
    fn campaign(&mut self, ids: &[u8]) {
        let mut queue = VecDeque::new();
        for &id in ids {
            self.peers.get_mut(&key(id)).unwrap().campaign();
            queue.extend(self.take(key(id)));
        }
        self.run(queue);
    }

    /// Delivers one message, then delivers until quiet.
    fn send(&mut self, message: Message) {
        self.run(VecDeque::from([message]));
    }

    /// Ticks one node. Its messages wait until the node next handles a message.
    fn tick(&mut self, id: u8, random: u64, times: u32) {
        let peer = self.peers.get_mut(&key(id)).unwrap();
        for _ in 0..times {
            peer.tick(random);
        }
    }

    /// Delivers what a node has waiting, then delivers until quiet.
    fn flush(&mut self, id: u8) {
        let queue = self.take(key(id)).into();
        self.run(queue);
    }

    fn cut(&mut self, a: u8, b: u8) {
        self.cuts.insert((key(a), key(b)));
        self.cuts.insert((key(b), key(a)));
    }

    fn isolate(&mut self, id: u8) {
        for other in 1..=u8::MAX {
            if other != id {
                self.cut(id, other);
            }
        }
    }

    fn recover(&mut self) {
        self.cuts.clear();
    }

    fn take(&mut self, id: node::Key) -> Vec<Message> {
        let ready = self.peers.get_mut(&id).unwrap().ready();
        let mut messages = self.disks.get_mut(&id).unwrap().store(ready);
        messages.retain(|message| !self.cuts.contains(&(message.from, message.to)));
        messages
    }

    fn run(&mut self, mut queue: VecDeque<Message>) {
        while let Some(message) = queue.pop_front() {
            let Some(peer) = self.peers.get_mut(&message.to) else {
                continue;
            };
            let to = message.to;
            peer.step(message).unwrap();
            queue.extend(self.take(to));
        }
    }

    #[track_caller]
    fn check(&self, id: u8, role: Role, term: u64) {
        let peer = self.peer(id);
        assert_eq!((peer.role(), peer.term()), (role, Term(term)), "node {id}");
    }
}

fn heartbeat(from: u8, to: u8, term: Term) -> Message {
    Message {
        from: key(from),
        to: key(to),
        term,
        body: Body::Heartbeat { commit: 0 },
    }
}

#[test]
fn leader_election() {
    let fresh = Hard::default();
    let cases = [
        (Network::of(3, &[1, 2, 3], fresh), Role::Leader, 1),
        (Network::of(3, &[1, 2], fresh), Role::Leader, 1),
        // An election that cannot complete leaves the node a pre-candidate and does
        // not advance the term.
        (Network::of(3, &[1], fresh), Role::PreCandidate, 0),
        (Network::of(4, &[1, 4], fresh), Role::PreCandidate, 0),
        (Network::of(5, &[1, 4, 5], fresh), Role::Leader, 1),
    ];
    for (i, (mut network, role, term)) in cases.into_iter().enumerate() {
        network.campaign(&[1]);
        let peer = network.peer(1);
        assert_eq!((peer.role(), peer.term()), (role, Term(term)), "case {i}");
    }
}

/// The last case of etcd's `testLeaderElection`: three logs are further along than
/// node 1's, in the same term, so node 1 gets rejections and not silence.
#[test]
fn leader_election_with_logs_ahead() {
    let voters = [1, 2, 3, 4, 5];
    let log = |index| Position {
        term: Term(1),
        index,
    };
    let mut network = Network::new([
        build(1, &voters, ELECTION, Hard::default(), Position::default()),
        build(2, &voters, ELECTION, at_term(1), log(1)),
        build(3, &voters, ELECTION, at_term(1), log(1)),
        build(4, &voters, ELECTION, at_term(1), log(2)),
        build(5, &voters, ELECTION, Hard::default(), Position::default()),
    ]);
    network.campaign(&[1]);
    network.check(1, Role::Follower, 1);
}

#[test]
fn single_node() {
    let mut network = Network::of(1, &[1], Hard::default());
    network.campaign(&[1]);
    network.check(1, Role::Leader, 1);
}

/// Node 1 at term 1 in `role`, with voters 1, 2, and 3, and an empty outbox. A
/// candidate and a leader are at term 2, because a campaign advances the term.
fn in_role(role: Role) -> Raft {
    let (mut raft, _) = build(1, &[1, 2, 3], ELECTION, at_term(1), Position::default());
    let reply = |body| Message {
        from: key(3),
        to: key(1),
        term: Term(2),
        body,
    };
    if role != Role::Follower {
        raft.campaign();
    }
    if matches!(role, Role::Candidate | Role::Leader) {
        raft.step(reply(Body::PreVoteReply { granted: true }))
            .unwrap();
    }
    if role == Role::Leader {
        raft.step(reply(Body::VoteReply { granted: true })).unwrap();
    }
    assert_eq!(raft.role(), role);
    drain(&mut raft);
    raft
}

const ROLES: [Role; 4] = [
    Role::Follower,
    Role::PreCandidate,
    Role::Candidate,
    Role::Leader,
];

/// etcd runs this without CheckQuorum, where a leader also grants the vote. With
/// CheckQuorum, a leader holds its lease and ignores the request.
#[test]
fn vote_from_any_state() {
    for role in ROLES {
        let mut raft = in_role(role);
        let old = raft.term();
        let new = Term(old.0 + 1);
        let last = Position {
            term: new,
            index: 42,
        };
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: new,
            body: Body::Vote { last },
        })
        .unwrap();
        let replies = drain(&mut raft);
        if role == Role::Leader {
            assert_eq!(replies, [], "{role:?}");
            assert_eq!((raft.role(), raft.term()), (Role::Leader, old));
            continue;
        }
        let reply = Message {
            from: key(1),
            to: key(2),
            term: new,
            body: Body::VoteReply { granted: true },
        };
        assert_eq!(replies, [reply], "{role:?}");
        assert_eq!(raft.role(), Role::Follower, "{role:?}");
        let hard = Hard {
            term: new,
            vote: Some(key(2)),
        };
        assert_eq!(raft.hard(), hard, "{role:?}");
    }
}

/// etcd runs this without CheckQuorum, where a leader also grants the PreVote. With
/// CheckQuorum, a leader holds its lease and ignores the request.
#[test]
fn prevote_from_any_state() {
    for role in ROLES {
        let mut raft = in_role(role);
        let hard = raft.hard();
        let new = Term(hard.term.0 + 1);
        let last = Position {
            term: new,
            index: 42,
        };
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: new,
            body: Body::PreVote { last },
        })
        .unwrap();
        let replies = drain(&mut raft);
        if role == Role::Leader {
            assert_eq!(replies, [], "{role:?}");
        } else {
            let reply = Message {
                from: key(1),
                to: key(2),
                term: new,
                body: Body::PreVoteReply { granted: true },
            };
            assert_eq!(replies, [reply], "{role:?}");
        }
        // A PreVote changes nothing.
        assert_eq!((raft.role(), raft.hard()), (role, hard), "{role:?}");
    }
}

/// The follower cases of etcd's `testRecvMsgVote`. The follower's log ends at index
/// 2, term 2. Returns whether each request was granted.
fn recv(request: fn(Position) -> Body) -> Vec<bool> {
    // (index, log term, the follower's earlier vote)
    let cases = [
        (0, 0, None),
        (0, 1, None),
        (0, 2, None),
        (0, 3, None),
        (1, 0, None),
        (1, 1, None),
        (1, 2, None),
        (1, 3, None),
        (2, 0, None),
        (2, 1, None),
        (2, 2, None),
        (2, 3, None),
        (3, 0, None),
        (3, 1, None),
        (3, 2, None),
        (3, 3, None),
        (3, 2, Some(2)),
        (3, 2, Some(1)),
    ];
    let own = Position {
        term: Term(2),
        index: 2,
    };
    cases
        .into_iter()
        .map(|(index, log_term, vote)| {
            let term = Term(log_term.max(2));
            let hard = Hard {
                term,
                vote: vote.map(key),
            };
            let (mut raft, _) = build(1, &[1], ELECTION, hard, own);
            let last = Position {
                term: Term(log_term),
                index,
            };
            raft.step(Message {
                from: key(2),
                to: key(1),
                term,
                body: request(last),
            })
            .unwrap();
            let [reply] = &drain(&mut raft)[..] else {
                panic!("expected one reply");
            };
            match &reply.body {
                Body::VoteReply { granted } | Body::PreVoteReply { granted } => {
                    *granted
                }
                Body::Vote { .. }
                | Body::PreVote { .. }
                | Body::Heartbeat { .. }
                | Body::HeartbeatReply
                | Body::Append { .. }
                | Body::AppendReply { .. }
                | Body::AppendReject { .. } => {
                    panic!("expected a reply, got {reply:?}")
                }
            }
        })
        .collect()
}

const RECV_GRANTED: [bool; 18] = [
    false, false, false, true, false, false, false, true, false, false, true, true,
    false, false, true, true, true, false,
];

#[test]
fn recv_vote() {
    assert_eq!(recv(|last| Body::Vote { last }), RECV_GRANTED);
}

#[test]
fn recv_prevote() {
    assert_eq!(recv(|last| Body::PreVote { last }), RECV_GRANTED);
}

/// etcd runs this without CheckQuorum, where node 2 rejects node 3's second PreVote
/// and node 3 goes back to follower. With CheckQuorum, nodes 1 and 2 hold their
/// leases and ignore it, so node 3 stays a pre-candidate. In both, node 3 does not
/// disturb the leader or the term.
#[test]
fn dueling_pre_candidates() {
    let mut network = Network::of(3, &[1, 2, 3], Hard::default());
    network.cut(1, 3);

    network.campaign(&[1]);
    network.campaign(&[3]);
    // Node 1 has the votes of 1 and 2. Node 2 rejects node 3's PreVote.
    network.check(1, Role::Leader, 1);
    network.check(3, Role::Follower, 1);

    network.recover();
    network.campaign(&[3]);
    network.check(1, Role::Leader, 1);
    network.check(2, Role::Follower, 1);
    network.check(3, Role::PreCandidate, 1);
}

/// etcd runs this without CheckQuorum, where node 2 replaces leader 1 with only a
/// campaign. With CheckQuorum, node 1 first loses its quorum and steps down.
#[test]
fn node_with_smaller_term_can_complete_election() {
    let mut network = Network::of(3, &[1, 2, 3], at_term(1));
    network.cut(1, 3);
    network.cut(2, 3);

    network.campaign(&[1]);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);

    network.campaign(&[3]);
    network.check(3, Role::PreCandidate, 1);

    network.cut(1, 2);
    network.tick(1, 0, 2 * ELECTION);
    network.flush(1);
    network.check(1, Role::Follower, 2);
    network.recover();
    network.cut(1, 3);
    network.cut(2, 3);

    network.campaign(&[2]);
    network.check(1, Role::Follower, 3);
    network.check(2, Role::Leader, 3);
    network.check(3, Role::PreCandidate, 1);

    // Bring back node 3 and cut off node 2, the leader, as if it crashed.
    network.recover();
    network.cut(2, 1);
    network.cut(2, 3);

    // Node 1 rejects the PreVote from the lower term, and node 3 takes its term.
    network.campaign(&[3]);
    network.check(3, Role::Follower, 3);
    network.campaign(&[1]);
    network.check(1, Role::Leader, 4);
    network.check(3, Role::Follower, 4);
}

/// After a split vote, the group completes an election in the next round.
#[test]
fn prevote_with_split_vote() {
    let mut network = Network::of(3, &[1, 2, 3], at_term(1));
    network.campaign(&[1]);

    // The leader goes down, and both followers campaign at once.
    network.isolate(1);
    network.campaign(&[2, 3]);
    network.check(2, Role::Candidate, 3);
    network.check(3, Role::Candidate, 3);

    // Node 2 times out first.
    network.campaign(&[2]);
    network.check(2, Role::Leader, 4);
    network.check(3, Role::Follower, 4);
}

#[test]
fn prevote_with_check_quorum() {
    let mut network = Network::of(3, &[1, 2, 3], at_term(1));
    network.campaign(&[1]);

    // Nodes 2 and 3 know the leader, and then lose it.
    network.isolate(1);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Follower, 2);

    // Node 2 ignores node 3's PreVote. Node 3, now a pre-candidate, grants node 2's.
    network.campaign(&[3]);
    network.campaign(&[2]);
    network.check(2, Role::Leader, 3);
    network.check(3, Role::Follower, 3);
}

/// PreVote with CheckQuorum stops a node from getting a PreVote while the voters
/// have heard from a leader within the election timeout. A voter that has not, or a
/// quorum of pre-candidates, can still elect a leader.
#[test]
fn prevote_checkquorum() {
    let mut network = Network::of(3, &[1, 2, 3], Hard::default());
    network.campaign(&[1]);
    network.check(1, Role::Leader, 1);

    // Node 2 fails to campaign and leaves node 1's leadership alone.
    network.campaign(&[2]);
    network.check(1, Role::Leader, 1);
    network.check(2, Role::PreCandidate, 1);
    network.check(3, Role::Follower, 1);

    // Node 2 has not heard from the leader for an election timeout, so it grants a
    // PreVote, and node 3 can hold an election.
    network.tick(2, 1, ELECTION);
    network.campaign(&[3]);
    network.check(1, Role::Follower, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Leader, 2);
    assert_eq!(network.peer(1).leader(), Some(key(3)));
    assert_eq!(network.peer(2).leader(), Some(key(3)));

    // The lease binds only followers. Nodes 1 and 2 can replace an active leader
    // when both campaign. Node 1 loses first, or the two would tie.
    network.campaign(&[1]);
    network.check(1, Role::PreCandidate, 2);
    network.check(3, Role::Leader, 2);

    network.campaign(&[2]);
    network.check(1, Role::Follower, 3);
    network.check(2, Role::Leader, 3);
    network.check(3, Role::Follower, 3);
}

/// Node 1 at term 1 as the leader of voters 1, 2, and 3, with an election timeout
/// of 5 ticks.
fn leader_of_three() -> Raft {
    let (mut raft, _) = build(1, &[1, 2, 3], 5, Hard::default(), Position::default());
    raft.campaign();
    for body in [
        Body::PreVoteReply { granted: true },
        Body::VoteReply { granted: true },
    ] {
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: Term(1),
            body,
        })
        .unwrap();
    }
    assert_eq!(raft.role(), Role::Leader);
    raft
}

#[test]
fn leader_stepdown_when_quorum_active() {
    let mut raft = leader_of_three();
    for _ in 0..=5 {
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: Term(1),
            body: Body::HeartbeatReply,
        })
        .unwrap();
        raft.tick(0);
    }
    assert_eq!(raft.role(), Role::Leader);
}

#[test]
fn leader_stepdown_when_quorum_lost() {
    let mut raft = leader_of_three();
    for _ in 0..=5 {
        raft.tick(0);
    }
    assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(1)));
    assert_eq!(raft.leader(), None);
}

/// etcd runs this without PreVote, where node 3 becomes a candidate at once. With
/// PreVote, node 3 stays a pre-candidate until node 2's lease ends.
#[test]
fn leader_superseding_with_check_quorum() {
    let mut network = Network::of(3, &[1, 2, 3], Hard::default());
    network.tick(2, 1, ELECTION);
    network.campaign(&[1]);
    network.check(1, Role::Leader, 1);
    network.check(3, Role::Follower, 1);

    // Node 2 ignores node 3, because it heard from the leader within the election
    // timeout.
    network.campaign(&[3]);
    network.check(1, Role::Leader, 1);
    network.check(3, Role::PreCandidate, 1);

    network.tick(2, 1, ELECTION);
    network.campaign(&[3]);
    network.check(1, Role::Follower, 2);
    network.check(3, Role::Leader, 2);
}

/// A node with a higher term than its group frees itself: its reply to the leader's
/// heartbeat makes the leader step down and take the higher term.
///
/// etcd runs this without PreVote, where node 3 raises its term with failed
/// campaigns after it holds the leader's first entry. With PreVote, a failed campaign
/// raises no term, so node 3 starts with the higher term and that entry on disk.
#[test]
fn free_stuck_candidate_with_check_quorum() {
    let voters = [1, 2, 3];
    let stuck = Hard {
        term: Term(3),
        vote: Some(key(3)),
    };
    let first = Position {
        term: Term(1),
        index: 1,
    };
    let mut network = Network::new([
        build(1, &voters, ELECTION, Hard::default(), Position::default()),
        build(2, &voters, ELECTION, Hard::default(), Position::default()),
        build(3, &voters, ELECTION, stuck, first),
    ]);
    network.isolate(3);
    network.campaign(&[1]);
    network.check(1, Role::Leader, 1);

    network.recover();
    network.send(heartbeat(1, 3, Term(1)));
    network.check(1, Role::Follower, 3);
    network.check(3, Role::Follower, 3);

    network.campaign(&[3]);
    network.check(3, Role::Leader, 4);
}

#[test]
fn non_promotable_voter_with_check_quorum() {
    let fresh = Hard::default();
    let mut network = Network::new([
        build(1, &[1, 2], ELECTION, fresh, Position::default()),
        // Node 2 is not in its own voter list.
        build(2, &[1], ELECTION, fresh, Position::default()),
    ]);
    network.tick(2, 0, 2 * ELECTION);
    network.check(2, Role::Follower, 0);

    network.campaign(&[1]);
    network.check(1, Role::Leader, 1);
    network.check(2, Role::Follower, 1);
    assert_eq!(network.peer(2).leader(), Some(key(1)));
}

/// A follower whose election times out just before a late heartbeat arrives does
/// not make the leader step down.
///
/// In etcd, node 3 is also behind in the log. Here all logs are equal, so the leases
/// alone protect the leader.
#[test]
fn disruptive_follower_prevote() {
    let mut network = Network::of(3, &[1, 2, 3], at_term(1));
    network.campaign(&[1]);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Follower, 2);

    // Node 3's last election tick passes before the leader's heartbeat arrives.
    network.tick(3, 2, ELECTION + 2);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::PreCandidate, 2);

    network.send(heartbeat(1, 3, Term(2)));
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Follower, 2);
}
