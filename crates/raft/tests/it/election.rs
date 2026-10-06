//! Election properties. The network checks election safety, the vote rule, and
//! leader completeness after every input; these properties add liveness.

use proptest::prelude::*;
use proptest::sample::Index;
use raft::{Answer, Body, Grant, Message, Position, Proof, Role, Term};
use types::node;

use crate::network::{Action, ELECTION, Network, run, run_of_many};

// The signed pre-vote that node `voter` grants to node `candidate` in `term`.
fn grant(voter: usize, candidate: usize, term: Term) -> Body {
    let (voter, candidate) = (Network::key(voter), Network::key(candidate));
    let signature = Network::signature(voter, Grant::PreVote, term, candidate);
    Body::PreVoteReply {
        answer: Answer::Granted(Some(signature)),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn a_term_has_one_leader_elected_by_logs_no_newer_than_its_own(
        (logs, actions) in run(),
    ) {
        let mut network = Network::new(&logs, 0);
        for action in &actions {
            network.apply(action);
        }
    }

    #[test]
    fn a_mended_network_elects_one_leader_and_keeps_it(
        (logs, actions) in run(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(&logs, random);
        let agreed = network.settle(&actions)?;
        for _ in 0..4 * ELECTION {
            network.round();
            prop_assert_eq!(network.agreed(), Some(agreed));
        }
    }

    #[test]
    fn a_node_that_was_cut_off_does_not_replace_the_leader(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
        pick in any::<Index>(),
    ) {
        let mut network = Network::new(&logs, random);
        let agreed = network.settle(&actions)?;
        let others = network.nodes.len() - 1;
        let follower = (agreed.0 + 1 + pick.index(others)) % network.nodes.len();
        network.cut[follower] = true;
        for _ in 0..4 * ELECTION {
            network.round();
        }
        network.cut[follower] = false;
        for _ in 0..4 * ELECTION {
            network.round();
        }
        prop_assert_eq!(network.agreed(), Some(agreed));
    }

    #[test]
    fn a_leader_without_a_quorum_steps_down_and_the_rest_elect_another(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(&logs, random);
        let (leader, _) = network.settle(&actions)?;
        network.cut[leader] = true;
        for _ in 0..2 * ELECTION {
            network.round();
        }
        prop_assert_ne!(network.nodes[leader].role(), Role::Leader);
        for _ in 0..20 * ELECTION {
            network.round();
        }
        let elected = network.nodes.iter().enumerate();
        let elected = elected.filter(|(_, node)| node.role() == Role::Leader);
        let elected: Vec<usize> = elected.map(|(at, _)| at).collect();
        prop_assert!(elected.len() == 1 && elected[0] != leader, "{elected:?}");
    }

    // An accepted gap (#719): a node counts a PreVote grant that its voter sent with
    // no lease, after the voter got its lease back.
    #[test]
    fn a_late_prevote_grant_costs_one_election(random in any::<u64>()) {
        let mut network = Network::new(&[Position::default(); 3], random);
        let (leader, term) = network.settle(&[])?;
        let (cut, other) = ((leader + 1) % 3, (leader + 2) % 3);
        network.cut[cut] = true;
        while network.nodes[cut].role() != Role::PreCandidate {
            network.round();
        }
        network.cut[cut] = false;
        network.deliver(&Message {
            from: Network::key(other),
            to: Network::key(cut),
            term: Term(term.0 + 1),
            body: grant(other, cut, Term(term.0 + 1)),
            proof: None,
        });
        prop_assert_eq!(network.nodes[cut].role(), Role::Candidate);
        for _ in 0..4 * ELECTION {
            network.round();
        }
        let agreed = network.agreed();
        prop_assert!(agreed.is_some_and(|(_, now)| now > term), "{agreed:?}");
    }
}

#[test]
fn a_group_keeps_its_leader_after_a_reply_from_a_node_that_is_not_a_peer() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let agreed = network.settle(&[]).unwrap();
    let follower = (agreed.0 + 1) % 3;
    let reply = Message {
        from: node::Key::from_u128(9),
        to: Network::key(follower),
        term: Term(agreed.1.0 + 1),
        body: Body::HeartbeatReply,
        proof: None,
    };
    network.nodes[follower].step(reply).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
    assert_eq!(network.agreed(), Some(agreed));
}

// An accepted gap (#352 item 2).
#[test]
fn one_message_in_the_last_term_stops_the_group_for_good() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let agreed = network.settle(&[]).unwrap();
    let follower = (agreed.0 + 1) % 3;
    let from = Network::key((agreed.0 + 2) % 3);
    network.deliver(&Message {
        from,
        to: Network::key(follower),
        term: Term(u64::MAX),
        body: Body::Heartbeat { commit: 0 },
        proof: Some(Proof {
            grant: Grant::Vote,
            candidate: from,
            voters: (0..3)
                .map(Network::key)
                .map(|voter| {
                    let signature =
                        Network::signature(voter, Grant::Vote, Term(u64::MAX), from);
                    (voter, Some(signature))
                })
                .collect(),
        }),
    });
    for _ in 0..10 * ELECTION {
        network.round();
    }
    for node in 0..3 {
        network.apply(&Action::Restart { node });
    }
    for _ in 0..10 * ELECTION {
        network.round();
    }
    let states = network.nodes.iter().map(|node| (node.role(), node.term()));
    let states: Vec<_> = states.collect();
    assert_eq!(states, [(Role::Follower, Term(u64::MAX)); 3]);
}

// A grant carries the term that its pre-campaign asks for. A grant of the current
// term is a late answer to a pre-campaign from the term before.
#[test]
fn a_prevote_grant_from_an_earlier_term_does_not_depose_the_leader() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let agreed = network.settle(&[]).unwrap();
    let (cut, other) = ((agreed.0 + 1) % 3, (agreed.0 + 2) % 3);
    network.cut[cut] = true;
    while network.nodes[cut].role() != Role::PreCandidate {
        network.round();
    }
    network.cut[cut] = false;
    network.deliver(&Message {
        from: Network::key(other),
        to: Network::key(cut),
        term: agreed.1,
        body: grant(other, cut, agreed.1),
        proof: None,
    });
    assert_eq!(network.nodes[cut].role(), Role::PreCandidate);
    for _ in 0..4 * ELECTION {
        network.round();
    }
    assert_eq!(network.agreed(), Some(agreed));
}

// Enough cases that each election-safety change tried in review fails a run.
const CASES: u32 = 2000;
