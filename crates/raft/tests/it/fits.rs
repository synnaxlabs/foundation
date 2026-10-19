//! A node sends no message that `Config::fits` refuses. An append with no proof
//! drops its last entries, down to one; a message with a proof drops its entries,
//! then the oldest links of its chain.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use proptest::prelude::*;
use raft::{
    Answer, Body, Change, Config, Data, Entry, Grant, Hard, Link, Message, Position,
    Proof, Raft, Role, Start, Term, Voters,
};
use types::node;

use crate::network::{NODES_MAX, Network, keyed, public, run, run_of_many};

const ELECTION: u32 = 10;

/// The bytes of `message` in a test byte form near that of `mesh`, with 64 bytes for
/// each signature, present or not.
pub(crate) fn bytes(message: &Message) -> usize {
    let proof = message.proof.as_ref().map_or(0, proof);
    let chain: usize = message.chain.iter().map(link).sum();
    41 + 1 + proof + 8 + chain + body(&message.body)
}

/// The bytes of `link`: its position, then its change.
pub(crate) fn link(link: &Link) -> usize {
    16 + change(&link.change)
}

fn proof(proof: &Proof) -> usize {
    1 + 16 + 8 + proof.voters.len() * (16 + 64)
}

fn change(change: &Change) -> usize {
    let keys = |keys: &BTreeSet<node::Key>| 8 + keys.len() * 16;
    let voters = &change.voters;
    let public_keys = change.public_keys.len() * 32;
    keys(&voters.incoming)
        + keys(&voters.outgoing)
        + public_keys
        + proof(&change.votes)
        + 64
}

fn body(body: &Body) -> usize {
    let answer = |answer: &Answer| match answer {
        Answer::Refused => 1,
        Answer::Granted(_) => 1 + 64,
    };
    1 + match body {
        Body::PreVote { .. } | Body::Vote { .. } => 16,
        Body::PreVoteReply { answer: a } | Body::VoteReply { answer: a } => answer(a),
        Body::HeartbeatReply => 0,
        Body::Heartbeat { .. }
        | Body::AppendReply { .. }
        | Body::AppendReject { .. } => 8,
        Body::Append { entries, .. } => {
            16 + 8 + entries.iter().map(entry).sum::<usize>()
        }
    }
}

fn entry(entry: &Entry) -> usize {
    16 + 1
        + match &entry.data {
            Data::Empty => 0,
            Data::Bytes(bytes) => 8 + bytes.len(),
            Data::Voters(change) => self::change(change),
        }
}

/// Whether `message` is at most 1472 bytes.
fn fits_1472(message: &Message) -> bool {
    bytes(message) <= 1472
}

/// Whether `message` with no link is at most the bytes of [`largest`], so an append
/// holds a few entries, and the chain of a node far behind is whole.
pub(crate) fn fits_largest(message: &Message) -> bool {
    let chain: usize = message.chain.iter().map(link).sum();
    bytes(message) - chain <= bytes(&largest())
}

/// The append of the largest change of a run: from {1} to each node.
fn largest() -> Message {
    let all = (1..=NODES_MAX).map(|id| key(u8::try_from(id).unwrap()));
    append_of(Voters {
        incoming: all.collect(),
        outgoing: set(&[1]),
    })
}

/// An append of the one change to `voters`, with no proof: a key for each added
/// voter, and a proof of a grant of each node of a run.
fn append_of(voters: Voters) -> Message {
    let all = (1..=NODES_MAX).map(|id| key(u8::try_from(id).unwrap()));
    let change = Change {
        public_keys: voters.added().map(|voter| (voter, public(voter))).collect(),
        voters,
        votes: Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: all.map(|voter| (voter, None)).collect(),
        },
        signature: None,
    };
    let at = |index| Position {
        term: Term(1),
        index,
    };
    Message {
        from: key(1),
        to: key(2),
        term: Term(1),
        body: Body::Append {
            prev: at(1),
            entries: vec![Entry {
                at: at(2),
                data: Data::Voters(change),
            }],
            commit: 1,
        },
        proof: None,
        chain: Vec::new(),
    }
}

fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}

fn node(id: u8) -> Raft {
    node_with(id, fits_1472)
}

fn node_with(id: u8, fits: fn(&Message) -> bool) -> Raft {
    let config = Config {
        key: key(id),
        election_ticks: ELECTION,
        heartbeat_ticks: 1,
        fits,
    };
    let start = Start {
        hard: Hard::default(),
        voters: Voters {
            incoming: set(&[1, 2, 3]),
            outgoing: BTreeSet::new(),
        },
        entries: Vec::new(),
        applied: 0,
    };
    Raft::new(config, start).unwrap()
}

// What a run sent, each message of which fits, and what each node committed.
#[derive(Default)]
struct Ran {
    sent: Vec<Message>,
    committed: BTreeMap<node::Key, Vec<Entry>>,
}

impl Ran {
    // Takes the `Ready` of `raft` into the run, and its messages into `queue`.
    fn take(&mut self, raft: &mut Raft, queue: &mut VecDeque<Message>) {
        let ready = raft.ready();
        let committed = self.committed.entry(raft.key()).or_default();
        committed.extend(ready.committed);
        for message in ready.messages {
            assert!(
                fits_1472(&message),
                "{} bytes: {message:?}",
                bytes(&message)
            );
            self.sent.push(message.clone());
            queue.push_back(message);
        }
    }
}

// Delivers every message between the nodes of `side` until none remains; the rest
// are lost.
fn run_among(nodes: &mut BTreeMap<node::Key, Raft>, side: &[u8], ran: &mut Ran) {
    let side = set(side);
    let mut queue = VecDeque::new();
    for id in &side {
        ran.take(nodes.get_mut(id).unwrap(), &mut queue);
    }
    while let Some(message) = queue.pop_front() {
        let Some(raft) = nodes
            .get_mut(&message.to)
            .filter(|_| side.contains(&message.to))
        else {
            continue;
        };
        raft.step(message).unwrap();
        ran.take(raft, &mut queue);
    }
}

// Ticks the nodes of `side` until one of them leads, and returns it.
fn elect(nodes: &mut BTreeMap<node::Key, Raft>, side: &[u8], ran: &mut Ran) -> u8 {
    for _ in 0..4 * ELECTION {
        for &id in side {
            nodes.get_mut(&key(id)).unwrap().tick(u64::from(id));
        }
        run_among(nodes, side, ran);
        let leads = |id: &&u8| nodes[&key(**id)].role() == Role::Leader;
        if let Some(&id) = side.iter().find(leads) {
            return id;
        }
    }
    panic!("no node of {side:?} leads");
}

// The entries of `message`, none when it is not an append.
fn entries(message: &Message) -> &[Entry] {
    match &message.body {
        Body::Append { entries, .. } => entries,
        _ => &[],
    }
}

// The indexes of the entries of each append that `ran` sent to `to`.
fn appended(ran: &Ran, to: u8) -> Vec<Vec<u64>> {
    let appends = ran
        .sent
        .iter()
        .filter(|message| message.to == key(to))
        .filter(|message| matches!(message.body, Body::Append { .. }));
    let indexes = |message| entries(message).iter().map(|entry| entry.at.index);
    appends.map(|message| indexes(message).collect()).collect()
}

// Three nodes that `fits`, with node 1 the leader of term 1 and each node at index 1.
fn led(fits: fn(&Message) -> bool) -> (BTreeMap<node::Key, Raft>, Ran) {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=3).map(|id| (key(id), node_with(id, fits))).collect();
    let mut ran = Ran::default();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run_among(&mut nodes, &[1, 2, 3], &mut ran);
    assert_eq!(nodes[&key(1)].role(), Role::Leader);
    (nodes, ran)
}

// The voters other than `id`.
fn others(id: u8) -> Vec<u8> {
    [1, 2, 3].into_iter().filter(|&other| other != id).collect()
}

#[test]
fn a_link_of_three_voters_in_each_set_is_457_bytes_and_489_when_it_adds_one() {
    let mut link = Link {
        at: Position {
            term: Term(1),
            index: 1,
        },
        change: Change {
            voters: Voters {
                incoming: set(&[1, 2, 3]),
                outgoing: set(&[1, 2, 3]),
            },
            public_keys: BTreeMap::new(),
            votes: Proof {
                grant: Grant::Vote,
                candidate: key(1),
                voters: [1, 2, 3].map(|id| (key(id), None)).into(),
            },
            signature: None,
        },
    };
    assert_eq!(self::link(&link), 457);
    link.change.voters.outgoing = set(&[1, 2, 4]);
    link.change.public_keys = keyed(&[3]);
    assert_eq!(self::link(&link), 489);
}

#[test]
fn the_append_of_the_largest_change_of_5_voters_is_821_bytes() {
    assert_eq!(NODES_MAX, 5);
    assert_eq!(bytes(&largest()), 821);
}

#[test]
fn no_change_of_a_run_has_a_larger_append_than_largest() {
    let nodes = |mask: u32| -> BTreeSet<node::Key> {
        let ids = 1..=u8::try_from(NODES_MAX).unwrap();
        ids.filter(|id| mask >> (id - 1) & 1 == 1)
            .map(key)
            .collect()
    };
    let sets = 1_u32 << NODES_MAX;
    let most = (1..sets)
        .flat_map(|incoming| (0..sets).map(move |outgoing| (incoming, outgoing)))
        .map(|(incoming, outgoing)| {
            bytes(&append_of(Voters {
                incoming: nodes(incoming),
                outgoing: nodes(outgoing),
            }))
        })
        .max();
    assert_eq!(most, Some(bytes(&largest())));
}

#[test]
fn fits_largest_holds_largest_and_refuses_one_byte_more() {
    let past = Message {
        body: Body::Append {
            prev: Position::default(),
            entries: vec![Entry {
                at: Position {
                    term: Term(1),
                    index: 1,
                },
                data: Data::Bytes(vec![0; 722]),
            }],
            commit: 0,
        },
        ..largest()
    };
    assert!(fits_largest(&largest()));
    assert_eq!(bytes(&past), bytes(&largest()) + 1);
    assert!(!fits_largest(&past));
}

#[test]
fn three_voters_elect_a_leader_after_a_change_in_each_of_3_terms() {
    let mut nodes: BTreeMap<node::Key, Raft> =
        (1..=3).map(|id| (key(id), node(id))).collect();
    let ran = &mut Ran::default();
    nodes.get_mut(&key(1)).unwrap().campaign();
    run_among(&mut nodes, &[1, 2, 3], ran);
    let mut leader = 1;
    for _ in 0..3 {
        let raft = nodes.get_mut(&key(leader)).unwrap();
        raft.propose_voters(keyed(&[1, 2, 3])).unwrap();
        run_among(&mut nodes, &[1, 2, 3], ran);
        leader = elect(&mut nodes, &others(leader), ran);
        run_among(&mut nodes, &[1, 2, 3], ran);
    }
    let links: Vec<Position> = ran.committed[&key(1)]
        .iter()
        .filter(|entry| matches!(entry.data, Data::Voters(_)))
        .map(|entry| entry.at)
        .collect();
    assert_eq!(links.len(), 6);
    let all: Vec<Link> = ran.committed[&key(1)]
        .iter()
        .filter_map(|entry| match &entry.data {
            Data::Voters(change) => Some(Link {
                at: entry.at,
                change: change.clone(),
            }),
            Data::Empty | Data::Bytes(_) => None,
        })
        .collect();

    let ran = &mut Ran::default();
    let elected = elect(&mut nodes, &others(leader), ran);
    let term = nodes[&key(elected)].term();
    let vote = ran
        .sent
        .iter()
        .find(|m| m.term == term && matches!(m.body, Body::Vote { .. }))
        .unwrap();
    let chain: Vec<Position> = vote.chain.iter().map(|link| link.at).collect();
    assert!(chain.len() < links.len(), "{chain:?}");
    assert!(links.ends_with(&chain), "{chain:?} of {links:?}");
    assert!(!chain.is_empty(), "{vote:?}");
    let dropped = &all[all.len() - chain.len() - 1];
    let mut longer = vote.clone();
    longer.chain.insert(0, dropped.clone());
    assert!(!fits_1472(&longer), "{} bytes", bytes(&longer));

    let at = nodes
        .get_mut(&key(elected))
        .unwrap()
        .propose(vec![7])
        .unwrap();
    run_among(&mut nodes, &[1, 2, 3], ran);
    // A heartbeat gives the followers the commit.
    nodes.get_mut(&key(elected)).unwrap().tick(0);
    run_among(&mut nodes, &[1, 2, 3], ran);
    for id in [1, 2, 3] {
        let committed = &ran.committed[&key(id)];
        assert_eq!(
            committed.last().map(|entry| entry.at),
            Some(at),
            "node {id}"
        );
    }
}

#[test]
fn an_append_with_no_proof_drops_its_last_entries() {
    let (mut nodes, mut ran) = led(|message| entries(message).len() <= 2);
    let leader = nodes.get_mut(&key(1)).unwrap();
    for data in 0..3 {
        leader.propose(vec![data]).unwrap();
    }
    run_among(&mut nodes, &[1, 2], &mut ran);
    // The reject of node 3 sends the leader back to index 2.
    let ran = &mut Ran::default();
    nodes.get_mut(&key(1)).unwrap().tick(0);
    run_among(&mut nodes, &[1, 2, 3], ran);
    let full = appended(ran, 3)
        .into_iter()
        .filter(|indexes| !indexes.is_empty());
    assert_eq!(full.collect::<Vec<_>>(), [vec![2, 3], vec![4]]);
    let unproven = ran.sent.iter().all(|message| message.proof.is_none());
    assert!(unproven, "{:?}", ran.sent);
}

#[test]
fn a_peer_that_lost_appends_of_one_entry_rejects_once() {
    let (mut nodes, mut ran) = led(|message| entries(message).len() <= 1);
    let leader = nodes.get_mut(&key(1)).unwrap();
    for data in 0..5 {
        leader.propose(vec![data]).unwrap();
    }
    run_among(&mut nodes, &[1, 2], &mut ran);
    for tick in 0..u64::from(ELECTION) {
        nodes.get_mut(&key(1)).unwrap().tick(tick);
        run_among(&mut nodes, &[1, 2, 3], &mut ran);
    }
    let rejects = ran
        .sent
        .iter()
        .filter(|message| matches!(message.body, Body::AppendReject { .. }));
    assert_eq!(rejects.count(), 1);
    let committed = ran.committed[&key(3)].last().map(|entry| entry.at.index);
    assert_eq!(committed, Some(6));
}

#[test]
#[should_panic(
    expected = "invariant: `Config::fits` holds an append of one entry with \
                           no proof, and each message with no entry and no link; to \
                           Key(2), term Term(1), 1 entries, no proof"
)]
fn a_leader_whose_fits_refuses_an_append_of_one_entry_panics() {
    let unproven =
        |message: &Message| message.proof.is_some() || entries(message).is_empty();
    let (mut nodes, _) = led(unproven);
    nodes.get_mut(&key(1)).unwrap().propose(vec![7]).unwrap();
    drop(nodes.get_mut(&key(1)).unwrap().ready());
}

#[test]
fn a_message_with_a_proof_drops_its_entries_and_keeps_its_chain() {
    let proven =
        |message: &Message| message.proof.is_none() || entries(message).is_empty();
    let (mut nodes, mut ran) = led(proven);
    let leader = nodes.get_mut(&key(1)).unwrap();
    leader.propose_voters(keyed(&[1, 2, 3])).unwrap();
    run_among(&mut nodes, &[1, 2, 3], &mut ran);
    let ran = &mut Ran::default();
    let elected = elect(&mut nodes, &[2, 3], ran);
    let leader = nodes.get_mut(&key(elected)).unwrap();
    leader.propose(vec![7]).unwrap();
    run_among(&mut nodes, &[elected], ran);
    let append = ran
        .sent
        .iter()
        .find(|message| matches!(message.body, Body::Append { .. }))
        .unwrap();
    assert!(append.proof.is_some(), "{append:?}");
    assert_eq!(entries(append), []);
    let chain: Vec<Position> = append.chain.iter().map(|link| link.at).collect();
    let at = |index| Position {
        term: Term(1),
        index,
    };
    assert_eq!(chain, [at(2), at(3)]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // `Network` checks that each message fits, and that an append with no proof
    // carries an entry when the leader has one to send.
    #[test]
    fn each_message_of_a_run_with_changes_fits((logs, actions) in run()) {
        let mut network = Network::with_fits(&logs, 0, fits_largest);
        for action in &actions {
            network.apply(action);
        }
    }

    #[test]
    fn a_proposal_to_the_leader_of_a_mended_network_of_small_messages_is_applied(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
    ) {
        let mut network = Network::with_fits(&logs, random, fits_largest);
        let (leader, _) = network.settle(&actions)?;
        let at = network.propose(leader).unwrap();
        let nodes: Vec<usize> = (0..network.nodes.len()).collect();
        network.apply_until(&nodes, at.index);
        for disk in &network.disks {
            prop_assert!(disk.applied >= at.index, "applied {}", disk.applied);
        }
    }
}

const CASES: u32 = 1000;
