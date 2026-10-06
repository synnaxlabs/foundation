//! A voter that is down through a configuration change is behind: it holds the old
//! configuration and refuses a leader whose votes are no quorum of it, until an
//! election whose grants are. A known gap: no proof covers the change itself.

use raft::{Position, Role, Term};

use crate::network::{Action, ELECTION, Network};

// Four nodes. Node 3 is cut off while the leader removes one other node, and the
// leader restarts, so a new leader wins with two votes of the three that remain.
// Returns the new leader, the other node that stays a voter, the index of the
// leave, and the term node 3 stays in.
fn shrink_behind_node_3(network: &mut Network) -> (usize, usize, u64, Term) {
    let (leader, term) = network.settle(&[]).unwrap();
    network.apply(&Action::Cut { node: 3 });
    let removed = (leader + 1) % 3;
    let kept = (leader + 2) % 3;
    let voters = (0..4).map(|node| node != removed).collect();
    network.apply(&Action::ChangeVoters {
        node: leader,
        voters,
    });
    let leave = network.disks[leader].last().index + 1;
    network.apply_until(&[leader, kept], leave);
    assert_eq!(network.disks[kept].applied, leave);
    network.apply(&Action::Restart { node: leader });
    let (next, _) = lead(network, term);
    assert!(network.behind(3, next));
    (next, kept, leave, term)
}

// Runs rounds until the voters agree on a leader of a term past `term`.
fn lead(network: &mut Network, term: Term) -> (usize, Term) {
    for _ in 0..10 * ELECTION {
        network.round();
        if let Some((leader, later)) = network.agreed()
            && later > term
        {
            return (leader, later);
        }
    }
    panic!("no leader past {term:?} after 10 election timeouts");
}

#[test]
fn a_voter_down_through_a_shrink_rejoins_when_the_leader_restarts() {
    let mut network = Network::new(&[Position::default(); 4], 0);
    let (leader, _, leave, before) = shrink_behind_node_3(&mut network);
    let term = network.nodes[leader].term();
    network.apply(&Action::Mend);
    for _ in 0..2 * ELECTION {
        network.round();
    }
    let node = &network.nodes[3];
    assert_eq!((node.term(), node.leader()), (before, None));
    assert_eq!(node.voters().incoming.len(), 4);
    assert_eq!(network.agreed(), Some((leader, term)));

    network.apply(&Action::Restart { node: leader });
    let (next, later) = lead(&mut network, term);
    assert!(!network.behind(3, next));
    network.apply_until(&[3], leave);
    let node = &network.nodes[3];
    assert_eq!(
        (node.term(), node.leader()),
        (later, Some(Network::key(next)))
    );
    assert_eq!(node.voters().incoming.len(), 3);
    assert!(network.disks[3].applied >= leave);
}

// The known gap: the voter behind cannot help the group once a second node fails,
// because no election can reach a quorum of the configuration it holds.
#[test]
fn a_group_with_a_voter_behind_dies_when_a_second_node_fails() {
    let mut network = Network::new(&[Position::default(); 4], 0);
    let (leader, kept, _, before) = shrink_behind_node_3(&mut network);
    network.apply(&Action::Mend);
    network.apply(&Action::Cut { node: kept });
    for _ in 0..10 * ELECTION {
        network.round();
    }
    assert!(network.nodes.iter().all(|node| node.role() != Role::Leader));
    assert_eq!(network.nodes[3].term(), before);
    assert!(network.behind(3, leader));
}
