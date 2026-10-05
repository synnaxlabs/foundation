//! Replication properties. The network checks log matching, leader completeness,
//! and state machine safety after every input; these properties add liveness.

use proptest::prelude::*;

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
        for _ in 0..4 * ELECTION {
            network.round();
            if network.disks.iter().all(|disk| disk.applied >= at.index) {
                break;
            }
        }
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
        for _ in 0..4 * ELECTION {
            network.round();
            if voters.iter().all(|&node| network.disks[node].applied >= at.index) {
                break;
            }
        }
        let configuration = network.nodes[leader].voters().clone();
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

const CASES: u32 = 1000;
