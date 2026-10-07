//! A node uses a configuration from the time it writes it. A follower that wrote a
//! configuration entry and restarted before it committed comes back under it, so
//! the acknowledgements it gave count toward one configuration only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{
    Body, Config, Data, Entry, Grant, Hard, Message, Position, Proof, Raft, Role,
    Start, Term, Voters,
};
use types::node;

use crate::network::change;

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}

fn entry(index: u64, data: Data) -> Entry {
    Entry {
        at: Position {
            term: Term(1),
            index,
        },
        data,
    }
}

fn node(id: u8, term: u64, voters: Voters, entries: Vec<Entry>, applied: u64) -> Raft {
    let config = Config {
        key: key(id),
        election_ticks: 10,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: Hard {
            term: Term(term),
            vote: None,
            leader: None,
            proof: None,
        },
        voters,
        entries,
        applied,
    };
    Raft::new(config, start).unwrap()
}

// Delivers every message between the nodes of `side` until none remain. A message
// to a node on the other side of the cut is lost. Returns what each node committed.
fn run(nodes: &mut BTreeMap<node::Key, Raft>, side: &[u8]) -> Vec<Entry> {
    let side = set(side);
    let mut committed = Vec::new();
    let mut queue: VecDeque<Message> = VecDeque::new();
    let take = |nodes: &mut BTreeMap<node::Key, Raft>,
                id: node::Key,
                queue: &mut VecDeque<Message>,
                committed: &mut Vec<Entry>| {
        let ready = nodes.get_mut(&id).unwrap().ready();
        committed.extend(ready.committed);
        queue.extend(ready.messages.into_iter().filter(|m| side.contains(&m.to)));
    };
    for &id in &side {
        take(nodes, id, &mut queue, &mut committed);
    }
    while let Some(message) = queue.pop_front() {
        let to = message.to;
        nodes.get_mut(&to).unwrap().step(message).unwrap();
        take(nodes, to, &mut queue, &mut committed);
    }
    committed
}

// Voters {1, 2, 3} moved to {1, 2, 3, 4, 5} through a joint phase: entry 2 is the
// joint configuration and entry 3 the new one. Both committed under the leader's
// quorums, and nodes 1, 4, and 5 applied them. Node 2 wrote both entries (its
// acknowledgements counted toward the commits) but restarted before it applied
// them. Node 3 holds only entry 1. Nodes 1, 4, and 5 are at `term`: above 1 after a
// failed election.
fn cluster(term: u64) -> BTreeMap<node::Key, Raft> {
    let old = Voters {
        incoming: set(&[1, 2, 3]),
        ..Voters::default()
    };
    let joint = Voters {
        incoming: set(&[1, 2, 3, 4, 5]),
        outgoing: set(&[1, 2, 3]),
    };
    let new = Voters {
        incoming: set(&[1, 2, 3, 4, 5]),
        ..Voters::default()
    };
    let log = vec![
        entry(1, Data::Empty),
        entry(2, change(key(1), joint)),
        entry(3, change(key(1), new.clone())),
    ];
    [
        node(1, term, new.clone(), log.clone(), 3),
        restarted(2, old.clone(), &log),
        node(3, 1, old, log[..1].to_vec(), 1),
        node(4, term, new.clone(), log.clone(), 3),
        node(5, term, new, log, 3),
    ]
    .into_iter()
    .map(|raft| (raft.key(), raft))
    .collect()
}

// Node `id`, a follower of leader 1 at term 1 that holds entry 1, takes entries 2 and
// 3 with commit index 2, writes them, and answers. Its `Ready` also commits entry 2,
// but the node crashes before it applies it. It restarts from what it wrote, with the
// voters it started from.
fn restarted(id: u8, old: Voters, log: &[Entry]) -> Raft {
    let (first, rest) = log.split_first().unwrap();
    let mut raft = node(id, 1, old.clone(), vec![first.clone()], 1);
    let append = Message {
        from: key(1),
        to: key(id),
        term: Term(1),
        body: Body::Append {
            prev: first.at,
            entries: rest.to_vec(),
            commit: 2,
        },
        proof: Some(Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [1, 2].map(|id| (key(id), None)).into(),
        }),
    };
    raft.step(append).unwrap();
    let ready = raft.ready();
    assert_eq!(ready.entries, rest);
    assert_eq!(ready.committed, rest[..1]);
    let mut disk = vec![first.clone()];
    disk.extend(ready.entries);
    node(id, 1, old, disk, 1)
}

#[test]
fn a_restarted_follower_cannot_elect_a_second_leader_for_a_term() {
    let mut nodes = cluster(1);
    // Partition {2, 3} | {1, 4, 5}. Each side campaigns once.
    nodes.get_mut(&key(2)).unwrap().campaign();
    run(&mut nodes, &[2, 3]);
    nodes.get_mut(&key(4)).unwrap().campaign();
    run(&mut nodes, &[1, 4, 5]);

    let leaders: Vec<(Term, node::Key)> = nodes
        .values()
        .filter(|raft| raft.role() == Role::Leader)
        .map(|raft| (raft.term(), raft.key()))
        .collect();
    let terms: BTreeSet<Term> = leaders.iter().map(|(term, _)| *term).collect();
    assert_eq!(
        terms.len(),
        leaders.len(),
        "two leaders in one term: {leaders:?}"
    );
}

#[test]
fn a_restarted_follower_cannot_commit_with_a_minority_of_the_new_voters() {
    let mut nodes = cluster(2);
    nodes.get_mut(&key(2)).unwrap().campaign();
    let a = run(&mut nodes, &[2, 3]);
    nodes.get_mut(&key(4)).unwrap().campaign();
    let b = run(&mut nodes, &[1, 4, 5]);
    // Two of the five voters commit nothing. Three elect node 4, and each of them
    // commits its first entry at index 4.
    assert_eq!(a, []);
    let at_4: Vec<&Entry> = b.iter().filter(|entry| entry.at.index == 4).collect();
    assert_eq!(at_4.len(), 3);
    assert!(at_4.iter().all(|entry| entry.data == Data::Empty));
}
