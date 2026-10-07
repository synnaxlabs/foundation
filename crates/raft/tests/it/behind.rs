//! A voter that is down through a configuration change holds the old configuration.
//! The new leader's chain proves the change, so the voter follows it, and helps
//! elect the next one.

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
    assert_eq!(network.nodes[3].term(), term);
    assert_eq!(network.nodes[3].voters().incoming.len(), 4);
    (next, kept, leave, term)
}

// Runs rounds until a node leads a term past `term`.
fn lead(network: &mut Network, term: Term) -> (usize, Term) {
    for _ in 0..10 * ELECTION {
        network.round();
        let leader = network
            .nodes
            .iter()
            .position(|node| node.role() == Role::Leader && node.term() > term);
        if let Some(leader) = leader {
            return (leader, network.nodes[leader].term());
        }
    }
    panic!("no leader past {term:?} after 10 election timeouts");
}

#[test]
fn a_voter_down_through_a_shrink_follows_the_new_leader_when_the_network_mends() {
    let mut network = Network::new(&[Position::default(); 4], 0);
    let (leader, _, leave, _) = shrink_behind_node_3(&mut network);
    let term = network.nodes[leader].term();
    network.apply(&Action::Mend);
    network.apply_until(&[3], leave);
    let node = &network.nodes[3];
    assert_eq!(
        (node.term(), node.leader()),
        (term, Some(Network::key(leader)))
    );
    assert_eq!(node.voters().incoming.len(), 3);
    assert!(network.disks[3].applied >= leave);
    assert_eq!(network.agreed(), Some((leader, term)));
}

#[test]
fn a_voter_behind_a_shrink_helps_elect_a_leader_when_a_second_node_fails() {
    let mut network = Network::new(&[Position::default(); 4], 0);
    let (leader, kept, leave, _) = shrink_behind_node_3(&mut network);
    let term = network.nodes[leader].term();
    network.apply(&Action::Mend);
    network.apply(&Action::Cut { node: kept });
    network.apply(&Action::Restart { node: leader });
    let (next, later) = lead(&mut network, term);
    assert_ne!(next, kept);
    network.apply_until(&[3], leave);
    let node = &network.nodes[3];
    assert_eq!(
        (node.term(), node.leader()),
        (later, Some(Network::key(next)))
    );
    assert_eq!(node.voters().incoming.len(), 3);
    assert!(network.disks[3].applied >= leave);
}
