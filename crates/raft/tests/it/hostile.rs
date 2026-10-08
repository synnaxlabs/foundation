//! What a message from a voter that does not lead does to another node.

use raft::{
    Body, Change, Claim, Entry, Error, Grant, Link, Message, Position, Proof, Raft,
    Role, Term, Voters,
};
use types::node;

use crate::change::{heartbeat, joining};
use crate::network::{Action, ELECTION, Network, change};

// An `Append` from `sender` in `term` that writes and commits one entry, which makes
// `victim` the only voter. Returns the entry too.
fn forged_append(
    network: &Network,
    sender: usize,
    victim: usize,
    term: Term,
) -> (Message, Entry) {
    let prev = network.disks[victim].entries.last().unwrap().at;
    let voters = Voters {
        incoming: [Network::key(victim)].into_iter().collect(),
        ..Voters::default()
    };
    let at = Position {
        term,
        index: prev.index + 1,
    };
    let entry = Entry {
        at,
        data: change(Network::key(sender), voters),
    };
    let append = Message {
        from: Network::key(sender),
        to: Network::key(victim),
        term,
        body: Body::Append {
            prev,
            entries: vec![entry.clone()],
            commit: at.index,
        },
        proof: None,
        chain: Vec::new(),
    };
    (append, entry)
}

fn unproven(term: Term, from: node::Key) -> Error {
    Error::Unproven { term, from }
}

// Checks that `node` refuses `append` with `refused`, and keeps its state and its log.
fn check_refused(
    node: &mut Raft,
    append: Message,
    refused: fn(Term, node::Key) -> Error,
) {
    let before = (node.role(), node.term(), node.leader(), node.hard());
    let (term, from) = (append.term, append.from);
    let err = node.step(append).unwrap_err();
    assert_eq!(err, refused(term, from));
    let from = format!("node {:032x}", from.as_u128());
    let text = match err {
        Error::SecondLeader { .. } => format!("{from} also claims to lead term {term}"),
        Error::Unproven { .. } => {
            format!("{from} claims term {term} with no proof this node accepts")
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(err.to_string(), text);
    assert_eq!(
        (node.role(), node.term(), node.leader(), node.hard()),
        before
    );
    assert_eq!(node.ready().entries, []);
}

#[test]
fn a_voter_that_does_not_lead_cannot_make_a_follower_commit_alone_in_its_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let (append, _) = forged_append(&network, sender, victim, term);
    let second = |term, from| Error::SecondLeader { term, from };
    check_refused(&mut network.nodes[victim], append, second);
    network.apply(&Action::Campaign { node: victim });
    // The network checks log matching and state machine safety from here.
    network.propose(leader).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
}

#[test]
fn a_voter_that_does_not_lead_cannot_make_a_follower_commit_alone_in_a_new_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let (append, _) = forged_append(&network, sender, victim, Term(term.0 + 1));
    check_refused(&mut network.nodes[victim], append, unproven);
    network.propose(leader).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
}

// A reply of a higher term with no proof does not end the leader's term, so the
// leader keeps its lease and refuses the forged `Append` like the reply.
#[test]
fn a_voter_that_does_not_lead_cannot_move_the_leader_to_a_new_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let sender = (leader + 1) % 3;
    let forged = Term(term.0 + 1);
    let (append, _) = forged_append(&network, sender, leader, forged);
    let reply = Message {
        body: Body::HeartbeatReply,
        ..append.clone()
    };
    let node = &mut network.nodes[leader];
    let from = Network::key(sender);
    assert_eq!(node.step(reply), Err(unproven(forged, from)));
    assert_eq!((node.role(), node.term()), (Role::Leader, term));
    check_refused(node, append, unproven);
}

#[test]
fn a_voter_that_does_not_lead_cannot_take_the_group_over_in_a_new_term() {
    let mut network = Network::new(&[Position::default(); 5], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 5;
    let sender = (leader + 2) % 5;
    let (append, entry) = forged_append(&network, sender, victim, Term(term.0 + 1));
    network.apply(&Action::Cut { node: sender });
    check_refused(&mut network.nodes[victim], append, unproven);
    network.propose(leader).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
    let taken = network
        .disks
        .iter()
        .filter(|disk| disk.entries.contains(&entry));
    assert_eq!(taken.count(), 0);
    assert_eq!(network.voters(leader).count(), 5);
}

// A new node must not take the empty set it started with as its committed
// configuration once it holds one.
#[test]
fn a_voter_cannot_move_a_new_node_that_holds_the_joint_configuration() {
    let (node, restarted) = joining();
    for mut node in [node, restarted] {
        check_refused(&mut node, heartbeat(2, u64::MAX, &[]), unproven);
        check_refused(&mut node, heartbeat(2, u64::MAX, &[2]), unproven);
    }
}

// A link carries only its leader's signature and the votes of its term, so a voter
// that led a term can sign a configuration entry it never wrote. This test pins the
// gap: #882 gives a link the signed acks of a quorum, and turns the step into
// `Error::Unproven`.
#[test]
fn a_voter_that_led_a_term_can_forge_a_link_to_itself_and_prove_any_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let from = Network::key(leader);
    let alone = Voters {
        incoming: [from].into_iter().collect(),
        ..Voters::default()
    };
    // The votes of the term, as the leader holds them.
    let vote = |voter| {
        let claim = Claim::Grant {
            voter,
            grant: Grant::Vote,
            term,
            candidate: from,
        };
        (voter, Some(Network::signature(&claim)))
    };
    let granted = (0..3).filter(|&node| network.nodes[node].hard().vote == Some(from));
    let votes = Proof {
        grant: Grant::Vote,
        candidate: from,
        voters: granted.map(|node| vote(Network::key(node))).collect(),
    };
    assert!(votes.voters.len() >= 2, "{votes:?}");
    let at = Position {
        term,
        index: network.disks[victim].last().index + 1,
    };
    let signature = Network::signature(&Claim::Change {
        leader: from,
        at,
        voters: &alone,
    });
    let link = Link {
        at,
        change: Change {
            voters: alone,
            votes,
            signature: Some(signature),
        },
    };
    let forged = Term(u64::MAX);
    let grant = Claim::Grant {
        voter: from,
        grant: Grant::Vote,
        term: forged,
        candidate: from,
    };
    let heartbeat = Message {
        from,
        to: Network::key(victim),
        term: forged,
        body: Body::Heartbeat { commit: 0 },
        proof: Some(Proof {
            grant: Grant::Vote,
            candidate: from,
            voters: [(from, Some(Network::signature(&grant)))].into(),
        }),
        chain: vec![link],
    };
    let node = &mut network.nodes[victim];
    assert_eq!(node.step(heartbeat), Ok(()));
    assert_eq!((node.term(), node.leader()), (forged, Some(from)));
}
