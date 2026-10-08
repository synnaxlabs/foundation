//! A node that a change removes gets the leave and its commit before the leader
//! releases it, so it stops campaigning. A node that misses its release campaigns
//! until the caller tells it that it is out.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{
    Answer, Body, Change, Config, Data, Entry, Error, Grant, Hard, Link, Message,
    Position, Proof, Raft, Role, Start, Term, Voters,
};
use types::node;

use crate::network::change;

const ELECTION: u32 = 10;

const REFUSED: Body = Body::PreVoteReply {
    answer: Answer::Refused,
};

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
    run_holding(nodes, &mut Vec::new(), |_| false)
}

// `run`, except that the messages `held` picks wait in `late`.
fn run_holding(
    nodes: &mut BTreeMap<node::Key, Raft>,
    late: &mut Vec<Message>,
    mut held: impl FnMut(&Message) -> bool,
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
        if held(&message) {
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

fn tick(nodes: &mut BTreeMap<node::Key, Raft>) {
    for node in nodes.values_mut() {
        node.tick(0);
    }
}

// A message and the round in which it was sent.
type Sent = (u32, Message);

fn sent(round: u32, from: u8, to: u8, term: u64, body: Body) -> Sent {
    let (from, to, term) = (key(from), key(to), Term(term));
    (
        round,
        Message {
            from,
            to,
            term,
            body,
            proof: None,
            chain: Vec::new(),
        },
    )
}

// The `PreVote`s that node `from` sends to each node of `to` when it campaigns.
fn campaign(round: u32, from: u8, to: &[u8], term: u64, end: Position) -> Vec<Sent> {
    to.iter()
        .map(|&id| sent(round, from, id, term, Body::PreVote { last: end }))
        .collect()
}

// Ticks every node and delivers every message, for `rounds` rounds. Returns what was
// sent to or from node `id`.
fn watch(nodes: &mut BTreeMap<node::Key, Raft>, rounds: u32, id: u8) -> Vec<Sent> {
    let mut seen = Vec::new();
    for round in 0..rounds {
        tick(nodes);
        let (_, sent) = run(nodes);
        seen.extend(
            sent.into_iter()
                .filter(|m| m.from == key(id) || m.to == key(id))
                .map(|m| (round, m)),
        );
    }
    seen
}

fn configs(entries: &[Entry]) -> Vec<Voters> {
    entries
        .iter()
        .filter_map(|entry| match &entry.data {
            Data::Voters(change) => Some(change.voters.clone()),
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
        tick(&mut nodes);
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
    run_holding(&mut nodes, &mut held, |m| m.from == key(3));
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    nodes
        .get_mut(&key(1))
        .unwrap()
        .propose_voters(set(&[1, 2]))
        .unwrap();
    run_holding(&mut nodes, &mut held, |m| m.from == key(3));
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
        tick(&mut nodes);
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
        tick(&mut nodes);
        let (_, sent) = run_holding(&mut nodes, &mut lost, |m| m.from == key(3));
        to_3.extend(sent.iter().filter(|m| m.to == key(3)).map(|_| round));
    }
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    assert_eq!(to_3, (0..ELECTION - 1).collect::<Vec<u32>>());
}

// Removes node `count` from nodes 1 to `count`, led by node 1. The answers of the
// other voters are late, so the removed node holds the leave before it commits, and
// the one heartbeat that brings it the commit is lost.
fn lose_the_release(count: u8) -> BTreeMap<node::Key, Raft> {
    let ids: Vec<u8> = (1..=count).collect();
    let (removed, kept) = (key(count), &ids[..ids.len() - 1]);
    let mut nodes: BTreeMap<node::Key, Raft> =
        ids.iter().map(|&id| (key(id), node(id, &ids))).collect();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run(&mut nodes);
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    let slow = |m: &Message| m.from != key(1) && m.from != removed;
    let mut late = Vec::new();
    nodes
        .get_mut(&key(1))
        .unwrap()
        .propose_voters(set(kept))
        .unwrap();
    run_holding(&mut nodes, &mut late, slow);
    let release = Body::Heartbeat { commit: 3 };
    let mut lost = Vec::new();
    while !late.is_empty() {
        for message in std::mem::take(&mut late) {
            let to = message.to;
            nodes.get_mut(&to).unwrap().step(message).unwrap();
            run_holding(&mut nodes, &mut late, |m| {
                slow(m) || m.to == removed && m.body == release
            });
        }
        let (to_removed, rest): (Vec<Message>, Vec<Message>) =
            late.drain(..).partition(|m| m.to == removed);
        lost.extend(to_removed);
        late = rest;
    }
    assert_eq!(
        lost,
        [Message {
            from: key(1),
            to: removed,
            term: Term(1),
            body: release,
            proof: None,
            chain: Vec::new(),
        }]
    );
    let new = Voters {
        incoming: set(kept),
        outgoing: BTreeSet::new(),
    };
    assert_eq!(nodes[&key(1)].voters(), &new);
    assert_eq!(nodes[&removed].voters(), &new);
    nodes
}

// Node 3 misses its release. It campaigns at each election timeout, and the voters,
// which hold the leader's lease, drop each campaign.
#[test]
fn leased_voters_drop_the_campaign_of_a_removed_node() {
    let mut nodes = lose_the_release(3);
    let with_3 = watch(&mut nodes, 3 * ELECTION, 3);
    let end = Position {
        term: Term(1),
        index: 3,
    };
    let campaigns: Vec<Sent> = (1..=3)
        .flat_map(|n| campaign(n * ELECTION - 1, 3, &[1, 2], 2, end))
        .collect();
    assert_eq!(with_3, campaigns);
    assert_eq!(
        (nodes[&key(1)].role(), nodes[&key(1)].term()),
        (Role::Leader, Term(1))
    );
    assert_eq!(nodes[&key(3)].role(), Role::PreCandidate);
}

// A known gap in `raft` alone: once no voter has a lease, the voters elect a removed
// node that missed its release. Node 4 holds the leave without its commit, so it
// campaigns, and its log is as long as theirs. After leader 1 fails, node 4 wins,
// commits an entry of its term, and steps down. Voters 2 and 3 follow it until their
// election timeout. `mesh` refuses such a request before `raft` sees it (#1105).
#[test]
fn the_voters_elect_a_removed_node_once_the_leader_fails() {
    let mut nodes = lose_the_release(4);
    // Node 4 times out first, and node 3 before node 2.
    let random = [(2, 6), (3, 3), (4, 0)];
    let mut gone = Vec::new();
    let mut history = Vec::new();
    for round in 0..4 * ELECTION {
        for (id, random) in random {
            nodes.get_mut(&key(id)).unwrap().tick(random);
        }
        run_holding(&mut nodes, &mut gone, |m| m.to == key(1));
        let state = |id| (nodes[&key(id)].term(), nodes[&key(id)].leader());
        let now = (state(2), state(3), nodes[&key(4)].role());
        if history.last().is_none_or(|(_, last)| *last != now) {
            history.push((round, now));
        }
    }
    let follow = |term, id| {
        let state = (Term(term), Some(key(id)));
        (state, state, Role::Follower)
    };
    assert_eq!(
        history,
        [(0, follow(1, 1)), (9, follow(2, 4)), (22, follow(3, 3))]
    );
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

// Node 2 leads {1, 2, 3} and removes node 3. Once the leave commits, each node holds
// node 3 as removed. A node that starts again from its disk holds the leave but no
// commit, so it holds node 3 as removed only once a leader gives it the commit index.
#[test]
fn removed_holds_once_a_leader_gives_the_commit_of_the_leave() {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=3).map(|id| (key(id), node(id, &[1, 2, 3]))).collect();
    nodes.get_mut(&key(2)).unwrap().campaign();
    let (mut committed, _) = run(&mut nodes);
    assert_eq!(nodes[&key(2)].role(), Role::Leader);
    nodes
        .get_mut(&key(2))
        .unwrap()
        .propose_voters(set(&[1, 2]))
        .unwrap();
    let (more, _) = run(&mut nodes);
    let removed = |raft: &Raft| [1, 3, 9].map(|id| raft.removed(key(id)));
    for id in 1..=3 {
        assert_eq!(removed(&nodes[&key(id)]), [false, true, false], "node {id}");
    }

    let mut disk = committed.remove(&key(2)).unwrap();
    disk.extend(more[&key(2)].iter().cloned());
    assert_eq!(configs(&disk).len(), 2);
    let restart = Start {
        hard: nodes[&key(2)].hard(),
        voters: Voters {
            incoming: set(&[1, 2, 3]),
            outgoing: BTreeSet::new(),
        },
        entries: disk,
        applied: 0,
    };
    let b = Raft::new(
        Config {
            key: key(2),
            election_ticks: ELECTION,
            heartbeat_ticks: 1,
        },
        restart,
    )
    .unwrap();
    assert_eq!(removed(&b), [false, false, false]);
    nodes.insert(key(2), b);
    nodes.get_mut(&key(1)).unwrap().campaign();
    run(&mut nodes);
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    assert_eq!(removed(&nodes[&key(2)]), [false, true, false]);
}

// Node 2 leads {1, 2, 3}, adds node 4, then removes it. No `Start.voters` holds node
// 4, so only the committed join holds it. From node 2's disk, a start at a commit
// below the join holds nothing about node 4, a start at the join holds it as a
// voter, and a start at the second leave holds it as removed.
#[test]
fn removed_holds_for_a_node_that_only_a_committed_entry_held() {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=4).map(|id| (key(id), node(id, &[1, 2, 3]))).collect();
    nodes.get_mut(&key(2)).unwrap().campaign();
    let (mut committed, _) = run(&mut nodes);
    let mut propose = |nodes: &mut BTreeMap<node::Key, Raft>, voters: &[u8]| {
        nodes
            .get_mut(&key(2))
            .unwrap()
            .propose_voters(set(voters))
            .unwrap();
        let (more, _) = run(nodes);
        committed
            .get_mut(&key(2))
            .unwrap()
            .extend(more[&key(2)].iter().cloned());
    };
    propose(&mut nodes, &[1, 2, 3, 4]);
    let removed = |raft: &Raft| [1, 4].map(|id| raft.removed(key(id)));
    for id in 1..=4 {
        assert_eq!(removed(&nodes[&key(id)]), [false, false], "node {id}");
    }
    propose(&mut nodes, &[1, 2, 3]);
    for id in 1..=4 {
        assert_eq!(removed(&nodes[&key(id)]), [false, true], "node {id}");
    }

    let disk = committed.remove(&key(2)).unwrap();
    let at: Vec<u64> = disk
        .iter()
        .filter(|entry| matches!(entry.data, Data::Voters(_)))
        .map(|entry| entry.at.index)
        .collect();
    assert_eq!(at.len(), 4);
    let cases = [
        (0, false),
        (at[0] - 1, false),
        (at[1], false),
        (at[3], true),
    ];
    for (applied, want) in cases {
        let start = Start {
            hard: nodes[&key(2)].hard(),
            voters: Voters {
                incoming: set(&[1, 2, 3]),
                outgoing: BTreeSet::new(),
            },
            entries: disk.clone(),
            applied,
        };
        let config = Config {
            key: key(2),
            election_ticks: ELECTION,
            heartbeat_ticks: 1,
        };
        let raft = Raft::new(config, start).unwrap();
        assert_eq!(removed(&raft), [false, want], "applied {applied}");
    }
}

// Whether a message crosses between the sides {1, 4} and {2, 3}.
fn crosses(message: &Message) -> bool {
    let side = |at: node::Key| at == key(1) || at == key(4);
    side(message.from) != side(message.to)
}

// Node 4 is removed. It holds the leave, as nodes 2 and 3 do, but no node holds its
// commit. Leader 1 sends node 4 no entry past the leave. Node 2 then wins term 2
// with node 3's vote. After the network mends, node 4 campaigns. The leased voters
// refuse it once at their term, then drop its campaigns, and node 2 keeps the lead.
#[test]
fn leased_voters_refuse_a_removed_node_that_missed_a_new_term() {
    let mut nodes: BTreeMap<node::Key, Raft> = (1..=4)
        .map(|id| (key(id), node(id, &[1, 2, 3, 4])))
        .collect();
    let mut lost = Vec::new();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run(&mut nodes);
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    nodes
        .get_mut(&key(1))
        .unwrap()
        .propose_voters(set(&[1, 2, 3]))
        .unwrap();
    // Nodes 2 and 3 take the leave, but their answers are lost, so the leave does
    // not commit. Node 4 takes the leave too.
    let (committed, _) = run_holding(&mut nodes, &mut lost, |m| {
        m.from != key(4) && m.body == Body::AppendReply { last: 3 }
    });
    assert_eq!(lost.len(), 2);
    // The joint entry is committed; the leave is not.
    assert_eq!(committed[&key(1)].len(), 1);
    assert_eq!(committed[&key(1)][0].at.index, 2);
    let new = Voters {
        incoming: set(&[1, 2, 3]),
        outgoing: BTreeSet::new(),
    };
    for id in 1..=4 {
        assert_eq!(nodes[&key(id)].voters(), &new, "node {id}");
    }
    lost.clear();

    // The network splits: {1, 4} and {2, 3}. Leader 1 proposes an entry that no
    // node gets: node 4 is on its side, but the entry is past the leave.
    nodes.get_mut(&key(1)).unwrap().propose(vec![7]).unwrap();
    run_holding(&mut nodes, &mut lost, crosses);
    assert_eq!(lost.len(), 2, "{lost:?}");
    lost.clear();

    // Nodes 2 and 3 time out and elect node 2 in term 2, which commits the leave.
    for _ in 0..3 * ELECTION {
        nodes.get_mut(&key(2)).unwrap().tick(0);
        nodes.get_mut(&key(3)).unwrap().tick(3);
        run_holding(&mut nodes, &mut lost, crosses);
    }
    assert_eq!(
        (nodes[&key(2)].role(), nodes[&key(2)].term()),
        (Role::Leader, Term(2))
    );
    assert_eq!(nodes[&key(3)].leader(), Some(key(2)));

    // The network mends.
    // Leader 1 heartbeats node 4 once before it learns of term 2.
    let with_4 = watch(&mut nodes, 8 * ELECTION, 4);
    let end = Position {
        term: Term(1),
        index: 3,
    };
    // Each refusal carries the proof of term 2: node 1 holds the votes of the
    // heartbeat that moved it, nodes 2 and 3 the pre-votes of the election.
    let mut expected = vec![
        sent(0, 1, 4, 1, Body::Heartbeat { commit: 2 }),
        sent(0, 4, 1, 1, Body::HeartbeatReply),
    ];
    expected.extend(campaign(ELECTION, 4, &[1, 2, 3], 2, end));
    expected.push(refusal(1, Grant::Vote));
    expected.extend([2, 3].map(|id| refusal(id, Grant::PreVote)));
    expected.extend((2..8).flat_map(|n| campaign(n * ELECTION, 4, &[1, 2, 3], 3, end)));
    assert_eq!(with_4, expected);
    assert_eq!(
        (nodes[&key(2)].role(), nodes[&key(2)].term()),
        (Role::Leader, Term(2))
    );
    assert_eq!(nodes[&key(1)].leader(), Some(key(2)));
    assert_eq!(nodes[&key(4)].role(), Role::PreCandidate);
}

// Leader 4 removes node 3 (leave at index 3), commits the leave, and starts a change
// that adds node 3 again (joint entry at index 4). Only node 3 gets that entry. Nodes
// 1 and 2 hold the leave but not its commit, so node 3 is still a peer of node 1.
// Leader 4 fails and node 1 wins term 2. Node 1 commits the leave and releases node
// 3. A released node knows that it is out and never campaigns.
#[test]
fn a_released_node_with_a_stale_entry_past_the_leave_never_campaigns() {
    let mut nodes: BTreeMap<node::Key, Raft> = (1..=4)
        .map(|id| (key(id), node(id, &[1, 2, 3, 4])))
        .collect();
    let mut lost = Vec::new();
    nodes.get_mut(&key(4)).unwrap().campaign();
    run(&mut nodes);
    assert_eq!(nodes[&key(4)].role(), Role::Leader);

    // Nodes 1 and 2 take the leave, but never its commit.
    let leader = nodes.get_mut(&key(4)).unwrap();
    leader.propose_voters(set(&[1, 2, 4])).unwrap();
    let to_voter = |m: &Message| m.to == key(1) || m.to == key(2);
    run_holding(&mut nodes, &mut lost, |m| {
        to_voter(m)
            && match m.body {
                Body::Append { commit, .. } | Body::Heartbeat { commit } => commit >= 3,
                _ => false,
            }
    });
    let left = Voters {
        incoming: set(&[1, 2, 4]),
        outgoing: BTreeSet::new(),
    };
    for id in 1..=4 {
        assert_eq!(nodes[&key(id)].voters(), &left, "node {id}");
    }

    // Leader 4 adds node 3 again. Only node 3 gets the joint entry at index 4.
    let leader = nodes.get_mut(&key(4)).unwrap();
    let joint = leader.propose_voters(set(&[1, 2, 3, 4])).unwrap();
    assert_eq!(joint.index, 4);
    run_holding(&mut nodes, &mut lost, to_voter);
    let again = Voters {
        incoming: set(&[1, 2, 3, 4]),
        outgoing: set(&[1, 2, 4]),
    };
    assert_eq!(nodes[&key(3)].voters(), &again);
    assert_eq!(nodes[&key(1)].voters(), &left);

    // Leader 4 fails. Nodes 1 and 2 are alone and elect node 1 in term 2. It
    // commits the leave with its own entry at index 4.
    let apart = |m: &Message| !(to_voter(m) && (m.from == key(1) || m.from == key(2)));
    for _ in 0..3 * ELECTION {
        nodes.get_mut(&key(1)).unwrap().tick(0);
        nodes.get_mut(&key(2)).unwrap().tick(7);
        run_holding(&mut nodes, &mut lost, apart);
        if nodes[&key(1)].role() == Role::Leader {
            break;
        }
    }
    assert_eq!(
        (nodes[&key(1)].role(), nodes[&key(1)].term()),
        (Role::Leader, Term(2))
    );

    // Node 3 can reach nodes 1 and 2 again. Node 4 stays down. Leader 1 heartbeats
    // node 3, sends it the append, and releases it with the commit of the leave.
    let down = |m: &Message| m.to == key(4) || m.from == key(4);
    let mut all = Vec::new();
    for _ in 0..2 {
        nodes.get_mut(&key(1)).unwrap().tick(0);
        all.extend(run_holding(&mut nodes, &mut lost, down).1);
    }
    let released = all.iter().any(|m| {
        m.to == key(3) && matches!(m.body, Body::Heartbeat { commit } if commit >= 3)
    });
    assert!(released, "{all:#?}");

    // Leader 1 sends node 3 nothing more: it is released. Node 3 knows that it is
    // out, so it sends nothing.
    let mut later = Vec::new();
    for round in 0..6 * ELECTION {
        for id in 1..=3 {
            nodes.get_mut(&key(id)).unwrap().tick(u64::from(round));
        }
        later.extend(run_holding(&mut nodes, &mut lost, down).1);
    }
    let to_3: Vec<&Message> = later.iter().filter(|m| m.to == key(3)).collect();
    let from_3: Vec<&Message> = later.iter().filter(|m| m.from == key(3)).collect();
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    assert_eq!((to_3, from_3.first().copied()), (vec![], None));
    assert_eq!(nodes[&key(3)].voters(), &left);
}

fn votes(candidate: u8, voters: &[u8]) -> Proof {
    Proof {
        grant: Grant::Vote,
        candidate: key(candidate),
        voters: voters.iter().map(|&id| (key(id), None)).collect(),
    }
}

// The refusal node `id` sends node 4 in term 2 with the proof of the term by nodes
// 2 and 3, and the chain of term 1: the joint entry and the leave to {1, 2, 3}.
fn refusal(id: u8, grant: Grant) -> Sent {
    let mut refusal = sent(ELECTION, id, 4, 2, REFUSED);
    refusal.1.proof = Some(Proof {
        grant,
        candidate: key(2),
        voters: [2, 3].map(|id| (key(id), None)).into(),
    });
    let joint = Voters {
        incoming: set(&[1, 2, 3]),
        outgoing: set(&[1, 2, 3, 4]),
    };
    refusal.1.chain = vec![
        link(2, joint),
        link(
            3,
            Voters {
                incoming: set(&[1, 2, 3]),
                outgoing: BTreeSet::new(),
            },
        ),
    ];
    refusal
}

// The change to `voters` that leader 1 wrote at `index` of term 1, elected by every
// node, as a link of a chain.
fn link(index: u64, voters: Voters) -> Link {
    Link {
        at: Position {
            term: Term(1),
            index,
        },
        change: Change {
            voters,
            votes: votes(1, &[1, 2, 3, 4]),
            signature: None,
        },
    }
}

// A heartbeat from `from` to node 4 that claims to lead `term` with the votes of
// `voters`.
pub(crate) fn heartbeat(from: u8, term: u64, voters: &[u8]) -> Message {
    Message {
        from: key(from),
        to: key(4),
        term: Term(term),
        body: Body::Heartbeat { commit: 0 },
        proof: Some(votes(from, voters)),
        chain: Vec::new(),
    }
}

// Node 4, started with no voters, after the first append of leader 1 in term 1 gave
// it the joint configuration that adds it, with its commit below that entry. Also
// gives node 4 restarted from what it wrote.
pub(crate) fn joining() -> (Raft, Raft) {
    let joint = Voters {
        incoming: set(&[1, 2, 3, 4]),
        outgoing: set(&[1, 2, 3]),
    };
    let config = Config {
        key: key(4),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
    };
    let mut node = Raft::new(config, Start::default()).unwrap();
    let at = |index| Position {
        term: Term(1),
        index,
    };
    let append = Message {
        from: key(1),
        to: key(4),
        term: Term(1),
        body: Body::Append {
            prev: Position::default(),
            entries: vec![
                Entry {
                    at: at(1),
                    data: Data::Empty,
                },
                Entry {
                    at: at(2),
                    data: change(key(1), joint.clone()),
                },
            ],
            commit: 1,
        },
        proof: Some(votes(1, &[1, 2])),
        chain: Vec::new(),
    };
    node.step(append).unwrap();
    assert_eq!(node.voters(), &joint);
    let ready = node.ready();
    let start = Start {
        hard: ready.hard.unwrap(),
        entries: ready.entries,
        applied: 1,
        ..Start::default()
    };
    let restarted = Raft::new(config, start).unwrap();
    assert_eq!(restarted.voters(), &joint);
    (node, restarted)
}

// The leader can fail before the joint entry commits, and a quorum of the outgoing
// set can elect a node that lacks it. The new node must follow that leader.
#[test]
fn a_new_node_follows_a_leader_elected_by_the_outgoing_set() {
    let (mut node, _) = joining();
    let unproven = Error::Unproven {
        term: Term(2),
        from: key(2),
    };
    assert_eq!(node.step(heartbeat(2, 2, &[2, 4])), Err(unproven));
    node.step(heartbeat(2, 2, &[2, 3])).unwrap();
    assert_eq!(
        (node.role(), node.term(), node.leader()),
        (Role::Follower, Term(2), Some(key(2)))
    );
}

// The leader of term 1 won under the founding configuration and then added node 4.
// Its votes still prove the term to node 4 while its commit is below the change.
#[test]
fn a_new_node_that_holds_the_leave_follows_a_leader_elected_before_the_change() {
    let joint = Voters {
        incoming: set(&[1, 2, 3, 4]),
        outgoing: set(&[1, 2, 3]),
    };
    let left = Voters {
        incoming: joint.incoming.clone(),
        ..Voters::default()
    };
    let entries = [Data::Empty, change(key(1), joint), change(key(1), left)]
        .into_iter()
        .zip(1..)
        .map(|(data, index)| Entry {
            at: Position {
                term: Term(1),
                index,
            },
            data,
        })
        .collect();
    let config = Config {
        key: key(4),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: Hard {
            term: Term(1),
            ..Hard::default()
        },
        entries,
        ..Start::default()
    };
    let mut node = Raft::new(config, start).unwrap();
    node.step(heartbeat(1, 1, &[1, 2])).unwrap();
    assert_eq!(
        (node.role(), node.term(), node.leader()),
        (Role::Follower, Term(1), Some(key(1)))
    );
}
