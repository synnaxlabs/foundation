//! What a message from a voter that does not lead does to another node.

use raft::{Body, Data, Entry, Error, Message, Position, Role, Term, Voters};

use crate::network::{Action, ELECTION, Network};

// An `Append` from `sender` in `term` that makes `victim` the only voter.
fn alone(network: &Network, sender: usize, victim: usize, term: Term) -> Message {
    let prev = network.disks[victim].entries.last().unwrap().at;
    let voters = Voters {
        incoming: [Network::key(victim)].into_iter().collect(),
        ..Voters::default()
    };
    let config = Entry {
        at: Position {
            term,
            index: prev.index + 1,
        },
        data: Data::Voters(voters),
    };
    Message {
        from: Network::key(sender),
        to: Network::key(victim),
        term,
        body: Body::Append {
            prev,
            entries: vec![config],
            commit: 0,
        },
    }
}

#[test]
fn a_voter_that_does_not_lead_cannot_make_a_follower_commit_alone() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let append = alone(&network, sender, victim, term);
    let err = network.nodes[victim].step(append).unwrap_err();
    let from = Network::key(sender);
    assert_eq!(err, Error::SecondLeader { term, from });
    network.apply(&Action::Campaign { node: victim });
    // The network checks log matching and state machine safety from here.
    network.propose(leader).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
}

// A known gap until #750: no node can tell the leader of a new term from a voter that
// does not lead.
#[test]
fn a_voter_that_does_not_lead_can_make_a_follower_take_a_term_of_its_own() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let forged = Term(term.0 + 1);
    let append = alone(&network, sender, victim, forged);
    let Body::Append { entries, .. } = append.body.clone() else {
        unreachable!()
    };
    network.nodes[victim].step(append).unwrap();
    let node = &mut network.nodes[victim];
    assert_eq!(
        (node.term(), node.leader()),
        (forged, Some(Network::key(sender)))
    );
    assert_eq!(node.ready().entries, entries);
}

// A known gap until #750. A leader steps down for a reply of a higher term, since a
// quorum may have moved on, so a lease cannot close the gap.
#[test]
fn a_voter_that_does_not_lead_can_make_the_leader_take_a_term_of_its_own() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let sender = (leader + 1) % 3;
    let forged = Term(term.0 + 1);
    let append = alone(&network, sender, leader, forged);
    let Body::Append { entries, .. } = append.body.clone() else {
        unreachable!()
    };
    let reply = Message {
        body: Body::HeartbeatReply,
        ..append.clone()
    };
    network.nodes[leader].step(reply).unwrap();
    network.nodes[leader].step(append).unwrap();
    let node = &mut network.nodes[leader];
    assert_eq!(node.role(), Role::Follower);
    assert_eq!(
        (node.term(), node.leader()),
        (forged, Some(Network::key(sender)))
    );
    assert_eq!(node.ready().entries, entries);
}
