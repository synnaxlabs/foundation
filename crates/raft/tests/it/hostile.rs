//! What a message from a voter that does not lead does to another node.

use raft::{Body, Data, Entry, Error, Message, Position, Raft, Role, Term, Voters};

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
    };
    (append, entry)
}

// A known gap until #750: checks that `node` takes `append`. It follows the sender in
// the append's term, and writes and commits `entry`.
fn check_taken(node: &mut Raft, append: Message, entry: Entry) {
    let (term, sender) = (append.term, append.from);
    node.step(append).unwrap();
    let state = (node.role(), node.term(), node.leader());
    assert_eq!(state, (Role::Follower, term, Some(sender)));
    let ready = node.ready();
    assert_eq!(ready.entries, [entry]);
    assert_eq!(ready.committed, ready.entries);
}

#[test]
fn a_voter_that_does_not_lead_cannot_make_a_follower_commit_alone_in_its_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let (append, _) = forged_append(&network, sender, victim, term);
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

#[test]
fn a_voter_that_does_not_lead_can_make_a_follower_commit_alone_in_a_new_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let (append, entry) = forged_append(&network, sender, victim, Term(term.0 + 1));
    check_taken(&mut network.nodes[victim], append, entry);
}

// A reply of a higher term ends the leader's term and its lease, so a lease that drops
// a forged `Append` cannot close the gap.
#[test]
fn a_voter_that_does_not_lead_can_make_the_leader_commit_alone_in_a_new_term() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let sender = (leader + 1) % 3;
    let forged = Term(term.0 + 1);
    let (append, entry) = forged_append(&network, sender, leader, forged);
    let reply = Message {
        body: Body::HeartbeatReply,
        ..append.clone()
    };
    let node = &mut network.nodes[leader];
    node.step(reply).unwrap();
    let state = (node.role(), node.term(), node.leader());
    assert_eq!(state, (Role::Follower, forged, None));
    check_taken(node, append, entry);
}
