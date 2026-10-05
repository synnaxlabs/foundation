//! A node that a change removes gets the leave and its commit before the leader
//! releases it, so it stops campaigning.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{Config, Data, Entry, Hard, Message, Raft, Role, Start, Term, Voters};
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
    run_holding(nodes, None)
}

// `run`, except that the messages one node sends wait in the given list.
fn run_holding(
    nodes: &mut BTreeMap<node::Key, Raft>,
    mut held: Option<(node::Key, &mut Vec<Message>)>,
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
        if let Some((from, late)) = &mut held
            && message.from == *from
        {
            late.push(message);
            continue;
        }
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

// Node 1 wins term 1 with node 2's vote while every message of node 3 is held, and
// removes node 3. Returns the nodes and the held messages, in order.
fn remove_node_3_while_held() -> (BTreeMap<node::Key, Raft>, Vec<Message>) {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=3).map(|id| (key(id), node(id, &[1, 2, 3]))).collect();
    let mut held = Vec::new();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run_holding(&mut nodes, Some((key(3), &mut held)));
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    nodes
        .get_mut(&key(1))
        .unwrap()
        .propose_voters(set(&[1, 2]))
        .unwrap();
    run_holding(&mut nodes, Some((key(3), &mut held)));
    let new = Voters {
        incoming: set(&[1, 2]),
        outgoing: BTreeSet::new(),
    };
    assert_eq!(nodes[&key(1)].voters(), &new);
    (nodes, held)
}

// Node 3's answers arrive after its removal. Node 3 holds the leader's log up to its
// answer to the first append, so it must still get the leave and its commit.
#[test]
fn a_removed_node_whose_answers_are_late_gets_the_leave_and_its_commit() {
    let (mut nodes, late) = remove_node_3_while_held();
    let new = Voters {
        incoming: set(&[1, 2]),
        outgoing: BTreeSet::new(),
    };

    // The late answers arrive, with everything they cause.
    for message in late {
        let to = message.to;
        nodes.get_mut(&to).unwrap().step(message).unwrap();
        run(&mut nodes);
    }
    assert_eq!(nodes[&key(3)].voters(), &new);
    for _ in 0..3 * ELECTION {
        for node in nodes.values_mut() {
            node.tick(0);
        }
        let (_, sent) = run(&mut nodes);
        assert!(sent.iter().all(|m| m.to != key(3)), "{sent:?}");
    }
    assert_eq!(nodes[&key(3)].role(), Role::Follower);
}

// Node 3 never answers. The leader removes it, and releases it when the first quorum
// check finds it silent, so it does not send to it for good.
#[test]
fn a_removed_node_that_stays_silent_is_released_at_a_quorum_check() {
    let (mut nodes, mut lost) = remove_node_3_while_held();
    let mut to_3 = Vec::new();
    for round in 0..3 * ELECTION {
        for node in nodes.values_mut() {
            node.tick(0);
        }
        let (_, sent) = run_holding(&mut nodes, Some((key(3), &mut lost)));
        to_3.extend(sent.iter().filter(|m| m.to == key(3)).map(|_| round));
    }
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    assert_eq!(to_3, (0..ELECTION - 1).collect::<Vec<u32>>());
}

// Delivers every message between `a` and `b` until none remains, and records what
// `b` writes to its disk.
fn exchange(a: &mut Raft, b: &mut Raft, disk_b: &mut Vec<Entry>) {
    loop {
        let mut flight = a.ready().messages;
        let ready = b.ready();
        write(disk_b, ready.entries);
        flight.extend(ready.messages);
        if flight.is_empty() {
            return;
        }
        for message in flight {
            let to = if message.to == a.key() {
                &mut *a
            } else {
                &mut *b
            };
            to.step(message).unwrap();
        }
    }
}

fn write(disk: &mut Vec<Entry>, entries: Vec<Entry>) {
    if let Some(first) = entries.first() {
        disk.truncate(usize::try_from(first.at.index - 1).unwrap());
        disk.extend(entries);
    }
}

// Node 2 leads {1, 2}, removes itself, and the joint entry commits. It writes the
// leave, then restarts before the leave reaches node 1. Node 1 holds the joint entry
// and needs node 2's vote, which node 2 refuses for its longer log. So node 2, whose
// leave is not committed, campaigns, wins, commits the leave, and steps down.
#[test]
fn a_removed_leader_that_restarts_before_the_leave_commits_finishes_the_change() {
    let mut a = node(1, &[1, 2]);
    let mut b = node(2, &[1, 2]);
    let mut disk_b = Vec::new();
    b.campaign();
    exchange(&mut a, &mut b, &mut disk_b);
    assert_eq!((a.role(), b.role()), (Role::Follower, Role::Leader));

    let joint = b.propose_voters(set(&[1])).unwrap();
    assert_eq!(joint.index, 2);
    let ready = b.ready();
    write(&mut disk_b, ready.entries);
    for message in ready.messages {
        a.step(message).unwrap();
    }
    for message in a.ready().messages {
        b.step(message).unwrap();
    }
    // The joint entry is committed; the leave is written but not sent.
    let ready = b.ready();
    write(&mut disk_b, ready.entries);
    assert_eq!(ready.committed.len(), 1);
    assert_eq!(disk_b.len(), 3);
    assert_eq!(b.voters().incoming, set(&[1]));
    drop(ready.messages);

    let restart = Start {
        hard: b.hard(),
        voters: Voters {
            incoming: set(&[1, 2]),
            outgoing: BTreeSet::new(),
        },
        entries: disk_b.clone(),
        applied: 2,
    };
    let mut b = Raft::new(
        Config {
            key: key(2),
            election_ticks: ELECTION,
            heartbeat_ticks: 1,
        },
        restart,
    )
    .unwrap();

    for round in 0..3 * ELECTION {
        a.tick(0);
        b.tick(u64::from(round));
        exchange(&mut a, &mut b, &mut disk_b);
        if a.role() == Role::Leader {
            break;
        }
    }
    // Node 2 led term 2, committed the leave, and stepped down; node 1 leads term 3.
    assert_eq!(
        (a.role(), a.term()),
        (Role::Leader, Term(3)),
        "{:?}",
        a.voters()
    );
    assert_eq!(b.role(), Role::Follower);
    let new = Voters {
        incoming: set(&[1]),
        outgoing: BTreeSet::new(),
    };
    assert_eq!(a.voters(), &new);
    assert_eq!(b.voters(), &new);
}
