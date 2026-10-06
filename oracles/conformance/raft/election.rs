//! Election scenarios ported from the tests of etcd/raft (Copyright 2015 The etcd
//! Authors, Apache License 2.0, see `LICENSE`). This file is modified from the etcd
//! source: `README.md` lists each source and the changes.

use raft::{Answer, Body, Grant, Hard, Message, Position, Raft, Role, Term};

use crate::common::{ELECTION, Network, at_term, build, heartbeat, key, proof};

fn drain(raft: &mut Raft) -> Vec<Message> {
    raft.ready().messages
}

#[test]
fn leader_election() {
    let fresh = Hard::default();
    let cases = [
        (Network::of(3, &[1, 2, 3], &fresh), Role::Leader, 1),
        (Network::of(3, &[1, 2], &fresh), Role::Leader, 1),
        // An election that cannot complete leaves the node a pre-candidate and does
        // not advance the term.
        (Network::of(3, &[1], &fresh), Role::PreCandidate, 0),
        (Network::of(4, &[1, 4], &fresh), Role::PreCandidate, 0),
        (Network::of(5, &[1, 4, 5], &fresh), Role::Leader, 1),
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
    let mut network = Network::of(1, &[1], &Hard::default());
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
        proof: None,
    };
    if role != Role::Follower {
        raft.campaign();
    }
    if matches!(role, Role::Candidate | Role::Leader) {
        raft.step(reply(Body::PreVoteReply {
            answer: Answer::Granted(None),
        }))
        .unwrap();
    }
    if role == Role::Leader {
        raft.step(reply(Body::VoteReply {
            answer: Answer::Granted(None),
        }))
        .unwrap();
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
        let pre_votes = proof(Grant::PreVote, 2, &[2, 3]);
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: new,
            body: Body::Vote { last },
            proof: Some(pre_votes.clone()),
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
            body: Body::VoteReply {
                answer: Answer::Granted(None),
            },
            proof: None,
        };
        assert_eq!(replies, [reply], "{role:?}");
        assert_eq!(raft.role(), Role::Follower, "{role:?}");
        let hard = Hard {
            term: new,
            vote: Some(key(2)),
            leader: None,
            proof: Some(pre_votes),
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
            proof: None,
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
                body: Body::PreVoteReply {
                    answer: Answer::Granted(None),
                },
                proof: None,
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
                vote: vote.map(key),
                ..at_term(term.0)
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
                proof: None,
            })
            .unwrap();
            let [reply] = &drain(&mut raft)[..] else {
                panic!("expected one reply");
            };
            match &reply.body {
                Body::VoteReply { answer } | Body::PreVoteReply { answer } => {
                    matches!(answer, Answer::Granted(_))
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
    let mut network = Network::of(3, &[1, 2, 3], &Hard::default());
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
    let mut network = Network::of(3, &[1, 2, 3], &at_term(1));
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
    let mut network = Network::of(3, &[1, 2, 3], &at_term(1));
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
    let mut network = Network::of(3, &[1, 2, 3], &at_term(1));
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
    let mut network = Network::of(3, &[1, 2, 3], &Hard::default());
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
        Body::PreVoteReply {
            answer: Answer::Granted(None),
        },
        Body::VoteReply {
            answer: Answer::Granted(None),
        },
    ] {
        raft.step(Message {
            from: key(2),
            to: key(1),
            term: Term(1),
            body,
            proof: None,
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
            proof: None,
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
    let mut network = Network::of(3, &[1, 2, 3], &Hard::default());
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
        leader: None,
        proof: Some(proof(Grant::PreVote, 3, &[1, 2, 3])),
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
        build(1, &[1, 2], ELECTION, fresh.clone(), Position::default()),
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
/// not make the leader step down. Node 3 is cut off while the leader commits three
/// entries, as in etcd, so its log is also behind. Nodes 1 and 2 hold their leases
/// and ignore its PreVote.
#[test]
fn disruptive_follower_prevote() {
    let mut network = Network::of(3, &[1, 2, 3], &at_term(1));
    network.campaign(&[1]);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Follower, 2);

    network.isolate(3);
    for _ in 0..3 {
        network.propose(1, b"somedata");
    }
    network.recover();
    assert_eq!(network.disk(1).last(), 4);
    assert_eq!(network.disk(3).last(), 1);

    network.tick(3, 2, ELECTION + 2);
    network.flush(3);
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::PreCandidate, 2);

    network.send(heartbeat(1, 3, Term(2)));
    network.check(1, Role::Leader, 2);
    network.check(2, Role::Follower, 2);
    network.check(3, Role::Follower, 2);
    assert_eq!(network.peer(3).leader(), Some(key(1)));
    assert_eq!(network.disk(3).last(), 4);
    assert_eq!(network.disk(3).committed.len(), 4);
}
