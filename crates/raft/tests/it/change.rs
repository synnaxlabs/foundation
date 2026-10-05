//! A node that a change removes gets the leave and its commit before the leader
//! forgets it, so it stops campaigning.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{Config, Data, Entry, Hard, Message, Raft, Role, Start, Voters};
use types::node;

const ELECTION: u32 = 10;

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}

fn node(id: u8, voters: &[u8]) -> Raft {
    let config = Config {
        key: key(id),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: Hard::default(),
        voters: Voters {
            incoming: set(voters),
            outgoing: BTreeSet::new(),
        },
        entries: Vec::new(),
        applied: 0,
    };
    Raft::new(config, start).unwrap()
}

// Delivers every message until none remains. Returns what each node committed and
// the messages the nodes sent, in order.
fn run(
    nodes: &mut BTreeMap<node::Key, Raft>,
) -> (BTreeMap<node::Key, Vec<Entry>>, Vec<Message>) {
    let mut committed: BTreeMap<node::Key, Vec<Entry>> = BTreeMap::new();
    let mut sent = Vec::new();
    let mut queue: VecDeque<Message> = VecDeque::new();
    let ids: Vec<node::Key> = nodes.keys().copied().collect();
    for id in ids {
        let ready = nodes.get_mut(&id).unwrap().ready();
        committed.entry(id).or_default().extend(ready.committed);
        sent.extend(ready.messages.iter().cloned());
        queue.extend(ready.messages);
    }
    while let Some(message) = queue.pop_front() {
        let to = message.to;
        nodes.get_mut(&to).unwrap().step(message).unwrap();
        let ready = nodes.get_mut(&to).unwrap().ready();
        committed.entry(to).or_default().extend(ready.committed);
        sent.extend(ready.messages.iter().cloned());
        queue.extend(ready.messages);
    }
    (committed, sent)
}

fn configs(entries: &[Entry]) -> Vec<Voters> {
    entries
        .iter()
        .filter_map(|entry| match &entry.data {
            Data::Voters(voters) => Some(voters.clone()),
            Data::Empty | Data::Bytes(_) => None,
        })
        .collect()
}

#[test]
fn a_removed_node_gets_the_leave_and_its_commit_then_stops_campaigning() {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=3).map(|id| (key(id), node(id, &[1, 2, 3]))).collect();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run(&mut nodes);
    assert_eq!(nodes[&key(1)].role(), Role::Leader);

    nodes
        .get_mut(&key(1))
        .unwrap()
        .propose_voters(set(&[1, 2]))
        .unwrap();
    let (committed, _) = run(&mut nodes);
    let joint = Voters {
        incoming: set(&[1, 2]),
        outgoing: set(&[1, 2, 3]),
    };
    let new = Voters {
        incoming: set(&[1, 2]),
        outgoing: BTreeSet::new(),
    };
    assert_eq!(configs(&committed[&key(3)]), [joint, new.clone()]);
    assert_eq!(nodes[&key(3)].voters(), &new);

    // The leader heartbeats the voters alone, and node 3 never campaigns.
    for _ in 0..3 * ELECTION {
        for node in nodes.values_mut() {
            node.tick(0);
        }
        let (_, sent) = run(&mut nodes);
        assert!(
            sent.iter()
                .all(|m| m.from == key(1) && m.to == key(2) || m.from == key(2)),
            "{sent:?}"
        );
    }
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    assert_eq!(nodes[&key(3)].role(), Role::Follower);
    assert_eq!(nodes[&key(3)].leader(), Some(key(1)));
}
