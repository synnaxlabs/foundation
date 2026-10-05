//! Replication properties. The network checks log matching, leader completeness,
//! and state machine safety after every input; these properties add liveness.

use proptest::prelude::*;
use raft::{Error, Position, Role};

use crate::network::{ELECTION, Network, run, run_of_many};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn logs_match_and_applied_entries_are_one_sequence(
        (logs, actions) in run(),
    ) {
        let mut network = Network::new(&logs, 0);
        for action in &actions {
            network.apply(action);
        }
        for disk in &network.disks {
            let applied = usize::try_from(disk.applied).unwrap();
            prop_assert_eq!(&disk.entries[..applied], &network.applied[..applied]);
        }
    }

    #[test]
    fn a_proposal_to_the_leader_of_a_mended_network_is_applied_everywhere(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(&logs, random);
        let (leader, _) = network.settle(&actions)?;
        let at = network.propose(leader).unwrap();
        let nodes: Vec<usize> = (0..network.nodes.len()).collect();
        network.apply_until(&nodes, at.index);
        let index = usize::try_from(at.index - 1).unwrap();
        prop_assert_eq!(network.applied[index].at, at);
        for disk in &network.disks {
            prop_assert!(disk.applied >= at.index, "applied {}", disk.applied);
        }
    }

    #[test]
    fn the_voters_of_a_mended_network_hold_the_leaders_configuration_and_log(
        (logs, actions) in run(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(&logs, random);
        let (leader, _) = network.settle(&actions)?;
        let at = network.propose(leader).unwrap();
        let voters: Vec<usize> = network.voters(leader).collect();
        network.apply_until(&voters, at.index);
        let configuration = network.nodes[leader].voters().clone();
        prop_assert!(configuration.outgoing.is_empty(), "{configuration:?}");
        for &node in &voters {
            prop_assert_eq!(network.nodes[node].voters(), &configuration);
            prop_assert!(network.disks[node].applied >= at.index, "node {node}");
            let applied = usize::try_from(network.disks[node].applied).unwrap();
            prop_assert_eq!(
                &network.disks[node].entries[..applied],
                &network.disks[leader].entries[..applied]
            );
        }
    }
}

// The disk owns durability (#352): a follower that lost synced entries refuses each
// heartbeat of the leader that counted them, and stays out while that leader leads.
#[test]
fn a_group_goes_on_without_a_follower_that_lost_synced_entries() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let agreed = network.settle(&[]).unwrap();
    let at = network.propose(agreed.0).unwrap();
    for _ in 0..4 * ELECTION {
        network.round();
    }
    let (lost, other) = ((agreed.0 + 1) % 3, (agreed.0 + 2) % 3);
    network.lose(lost);
    for _ in 0..10 * ELECTION {
        network.round();
    }
    let leader = &network.nodes[agreed.0];
    assert_eq!((leader.role(), leader.term()), (Role::Leader, agreed.1));
    let follows = |node: usize| {
        let node = &network.nodes[node];
        (node.term(), node.leader())
    };
    assert_eq!(follows(other), (agreed.1, Some(leader.key())));
    assert_eq!(follows(lost), (agreed.1, None));
    network.refused.dedup();
    let refused = Error::IndexPastLog {
        index: at.index,
        last: 0,
    };
    assert_eq!(network.refused, [refused]);
}

const CASES: u32 = 1000;
