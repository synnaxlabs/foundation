//! A leader that restarts with a configuration entry it wrote and never sent. The
//! case of the red team on #881, comment 6030871431.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{Config, Raft, Role, Start, Voters};
use types::node;

const ELECTION: u32 = 10;

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn id(key: node::Key) -> u8 {
    (1..=3).find(|&id| self::key(id) == key).unwrap()
}

fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}

fn plain(ids: &[u8]) -> Voters {
    joint(ids, &[])
}

fn joint(incoming: &[u8], outgoing: &[u8]) -> Voters {
    Voters {
        incoming: set(incoming),
        outgoing: set(outgoing),
    }
}

// Voters {2,3} move to {1,3}. Leader 3 writes the leave to disk, stops before it
// sends it, and restarts. After that no node is down and no message is lost.
#[test]
fn a_leader_that_restarts_with_an_unsent_leave_leads_again() {
    let config = |id| Config {
        key: key(id),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
    };
    let start = |ids: &[u8]| Start {
        voters: plain(ids),
        ..Start::default()
    };
    let mut nodes: BTreeMap<u8, Raft> = [(1, &[][..]), (2, &[2, 3]), (3, &[2, 3])]
        .into_iter()
        .map(|(id, ids)| (id, Raft::new(config(id), start(ids)).unwrap()))
        .collect();
    // The disk of node 3.
    let mut disk = start(&[2, 3]);
    let mut flight = VecDeque::new();
    let mut errors = Vec::new();
    nodes.get_mut(&3).unwrap().campaign();
    let mut proposed = false;
    let mut rounds = 0..40 * u64::from(ELECTION);
    loop {
        for (&at, node) in &mut nodes {
            let ready = node.ready();
            if at == 3 {
                if let Some(hard) = ready.hard {
                    disk.hard = hard;
                }
                if let Some(first) = ready.entries.first() {
                    let kept = usize::try_from(first.at.index - 1).unwrap();
                    disk.entries.truncate(kept);
                    disk.entries.extend(ready.entries);
                }
                disk.applied += ready.committed.len() as u64;
            }
            flight.extend(ready.messages);
        }
        let leader = nodes.get_mut(&3).unwrap();
        if !proposed && leader.role() == Role::Leader {
            leader.propose_voters(set(&[1, 3])).unwrap();
            proposed = true;
            continue;
        }
        if rounds.start == 0 && leader.voters() == &plain(&[1, 3]) {
            // The leave is on disk and in no message that arrives.
            assert_eq!(nodes[&1].voters(), &joint(&[1, 3], &[2, 3]));
            flight.clear();
            nodes.insert(3, Raft::new(config(3), disk.clone()).unwrap());
            rounds.start = 1;
        }
        if let Some(message) = flight.pop_front() {
            let to = nodes.get_mut(&id(message.to)).unwrap();
            errors.extend(to.step(message).err());
        } else if let Some(round) = rounds.next().filter(|_| rounds.start > 0) {
            for (&id, node) in &mut nodes {
                node.tick(round * 7 + u64::from(id) * 3);
            }
        } else {
            break;
        }
    }
    let leaders = nodes.values().filter(|node| node.role() == Role::Leader);
    assert_eq!(
        leaders.count(),
        1,
        "{:?} {:?}",
        errors.len(),
        errors.first()
    );
}
