//! What a message from a voter that does not lead does to another node.

use std::collections::BTreeSet;

use raft::{
    Body, Config, Data, Entry, Error, Grant, Hard, Message, Position, Proof, Raft,
    Role, Start, Term, Voters,
};
use types::node;

use crate::network::{Action, ELECTION, Network};

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
        data: Data::Voters(voters),
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

fn key(id: u128) -> node::Key {
    node::Key::from_u128(id)
}

fn set(ids: &[u128]) -> BTreeSet<node::Key> {
    ids.iter().map(|&id| key(id)).collect()
}

// The joint configuration that adds node 4 to voters 1, 2 and 3.
fn joint() -> Voters {
    Voters {
        incoming: set(&[1, 2, 3, 4]),
        outgoing: set(&[1, 2, 3]),
    }
}

// A message from `from` to node 4 that claims to lead `term` with the votes of
// `voters`.
fn heartbeat(from: u128, term: u64, voters: &[u128]) -> Message {
    Message {
        from: key(from),
        to: key(4),
        term: Term(term),
        body: Body::Heartbeat { commit: 0 },
        proof: Some(Proof {
            grant: Grant::Vote,
            candidate: key(from),
            voters: voters.iter().map(|&id| (key(id), None)).collect(),
        }),
    }
}

// Node 4, started with no voters, after the first append of leader 1 in term 1
// gave it the joint configuration, with its commit below that entry. Also gives
// node 4 restarted from what it wrote.
fn joining() -> (Raft, Raft) {
    let config = Config {
        key: key(4),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: Hard::default(),
        voters: Voters::default(),
        entries: Vec::new(),
        applied: 0,
    };
    let mut node = Raft::new(config, start).unwrap();
    let at = |index| Position {
        term: Term(1),
        index,
    };
    let mut append = heartbeat(1, 1, &[1, 2]);
    append.body = Body::Append {
        prev: Position::default(),
        entries: vec![
            Entry {
                at: at(1),
                data: Data::Empty,
            },
            Entry {
                at: at(2),
                data: Data::Voters(joint()),
            },
        ],
        commit: 1,
    };
    node.step(append).unwrap();
    assert_eq!(node.voters(), &joint());
    let ready = node.ready();
    let start = Start {
        hard: ready.hard.unwrap(),
        voters: Voters::default(),
        entries: ready.entries,
        applied: 1,
    };
    let restarted = Raft::new(config, start).unwrap();
    assert_eq!(restarted.voters(), &joint());
    (node, restarted)
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

// The leader can fail before the joint entry commits, and a quorum of the outgoing
// set can elect a node that lacks it. The new node must follow that leader.
#[test]
fn a_new_node_follows_a_leader_elected_by_the_outgoing_set() {
    let (mut node, _) = joining();
    check_refused(&mut node, heartbeat(2, 2, &[2, 4]), unproven);
    node.step(heartbeat(2, 2, &[2, 3])).unwrap();
    assert_eq!(
        (node.role(), node.term(), node.leader()),
        (Role::Follower, Term(2), Some(key(2)))
    );
}
