//! A disk that lost entries it synced. `raft` is safe only when a disk keeps what it
//! synced (#352 item 3); these tests pin what it does when one does not.

use raft::{Error, Position, Role};

use crate::network::{Action, ELECTION, Network};

#[test]
fn a_group_goes_on_without_a_follower_that_lost_its_committed_entries() {
    let mut network = Network::new(&[Position::default(); 3], 0);
    let (leader, term) = network.settle(&[]).unwrap();
    let (wiped, other) = ((leader + 1) % 3, (leader + 2) % 3);
    let at = network.propose(leader).unwrap();
    network.apply_until(&[wiped, other], at.index);
    network.wipe(wiped, 0);
    for _ in 0..10 * ELECTION {
        network.round();
    }
    let next = network.propose(leader).unwrap();
    network.apply_until(&[other], next.index);
    let node = &network.nodes[leader];
    assert_eq!((node.role(), node.term()), (Role::Leader, term));
    assert_eq!(network.disks[other].applied, next.index);
    assert_eq!(network.disks[wiped].entries, []);
    network.refused.dedup();
    let expected = Error::IndexPastLog {
        index: at.index,
        last: 0,
    };
    assert_eq!(network.refused, [expected]);
}

// An accepted gap (#352 item 3).
#[test]
fn a_leader_commits_with_entries_that_a_follower_lost() {
    let mut network = Network::new(&[Position::default(); 5], 0);
    let (leader, _) = network.settle(&[]).unwrap();
    let [wiped, held, rest @ ..] = [1, 2, 3, 4].map(|step| (leader + step) % 5);
    for node in [held, rest[0], rest[1]] {
        network.apply(&Action::Cut { node });
    }
    network.propose(leader).unwrap();
    let last = network.propose(leader).unwrap();
    network.round();
    assert_eq!(network.disks[wiped].last(), last);
    network.wipe(wiped, 1);
    network.apply(&Action::Mend);
    for node in rest {
        network.apply(&Action::Cut { node });
    }
    network.round();
    network.round();
    let holders = (0..5).filter(|&node| network.disks[node].last() == last);
    assert_eq!(holders.count(), 2);
    assert_eq!(network.disks[leader].applied, last.index);
    assert_eq!(network.nodes[wiped].leader(), Some(Network::key(leader)));
    assert_eq!(network.refused, []);
    for _ in 0..ELECTION {
        network.round();
    }
    network.refused.dedup();
    let expected = Error::IndexPastLog {
        index: last.index,
        last: 1,
    };
    assert_eq!(network.refused, [expected]);
}
