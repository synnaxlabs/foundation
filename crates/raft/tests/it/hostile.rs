//! What a message from a voter that does not lead does to a follower.

use raft::{Body, Data, Entry, Error, Message, Position, Voters};

use crate::network::{Action, ELECTION, Network};

#[test]
fn a_voter_that_does_not_lead_cannot_make_a_follower_commit_alone() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let victim = (leader + 1) % 3;
    let sender = (leader + 2) % 3;
    let prev = network.disks[victim].entries.last().unwrap().at;
    let alone = Voters {
        incoming: [Network::key(victim)].into_iter().collect(),
        ..Voters::default()
    };
    let config = Entry {
        at: Position {
            term,
            index: prev.index + 1,
        },
        data: Data::Voters(alone),
    };
    let append = Message {
        from: Network::key(sender),
        to: Network::key(victim),
        term,
        body: Body::Append {
            prev,
            entries: vec![config],
            commit: 0,
        },
    };
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
