//! A network of nodes that cuts nodes off and loses, delays, reorders, and repeats
//! messages, with nodes that restart from a modeled disk. It checks the safety
//! properties of Raft after every input.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::sample::Index;
use raft::{
    Body, Config, Data, Entry, Error, Hard, Message, Position, Raft, Role, Start, Term,
    Voters,
};
use types::node;

pub(crate) const ELECTION: u32 = 5;
pub(crate) const HEARTBEAT: u32 = 2;

#[derive(Clone, Debug)]
pub(crate) enum Action {
    Tick { node: usize, random: u64 },
    Campaign { node: usize },
    Propose { node: usize },
    Restart { node: usize },
    Deliver { picks: Vec<Index> },
    Repeat { pick: Index },
    Lose { pick: Index },
    Cut { node: usize },
    Mend,
    // A proposal to `node` of the voter set of the flagged nodes.
    ChangeVoters { node: usize, members: Vec<bool> },
}

fn action(nodes: usize, changes: bool) -> impl Strategy<Value = Action> {
    let node = 0..nodes;
    let pick = any::<Index>;
    let change = (node.clone(), prop::collection::vec(any::<bool>(), nodes))
        .prop_map(|(node, members)| Action::ChangeVoters { node, members });
    prop_oneof![
        u32::from(changes) => change,
        4 => (node.clone(), any::<u64>())
            .prop_map(|(node, random)| Action::Tick { node, random }),
        1 => node.clone().prop_map(|node| Action::Campaign { node }),
        3 => node.clone().prop_map(|node| Action::Propose { node }),
        1 => node.clone().prop_map(|node| Action::Restart { node }),
        6 => prop::collection::vec(pick(), 1..8)
            .prop_map(|picks| Action::Deliver { picks }),
        1 => pick().prop_map(|pick| Action::Repeat { pick }),
        1 => pick().prop_map(|pick| Action::Lose { pick }),
        1 => node.prop_map(|node| Action::Cut { node }),
        1 => Just(Action::Mend),
    ]
}

// An empty log ends at the zero position; any other ends in a term above zero.
fn position() -> impl Strategy<Value = Position> {
    let filled = (1..3u64, 1..3u64).prop_map(|(term, index)| Position {
        term: Term(term),
        index,
    });
    prop_oneof![1 => Just(Position::default()), 3 => filled]
}

/// The last log position of each node, and the actions to run, membership changes
/// among them.
pub(crate) fn run() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    let nodes = prop_oneof![1 => 1..=2_usize, 5 => Just(3), 1 => Just(4), 4 => Just(5)];
    run_of(nodes, true)
}

/// A run of a group that keeps a quorum when one node is cut off: no membership
/// changes.
pub(crate) fn run_of_many() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    run_of(
        prop_oneof![5 => Just(3_usize), 1 => Just(4), 4 => Just(5)],
        false,
    )
}

fn run_of(
    nodes: impl Strategy<Value = usize>,
    changes: bool,
) -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    nodes.prop_flat_map(move |nodes| {
        (
            prop::collection::vec(position(), nodes),
            prop::collection::vec(action(nodes, changes), 0..400),
        )
    })
}

/// What one node has on disk: what each `Ready` told the caller to write and apply.
#[derive(Clone, Debug)]
pub(crate) struct Disk {
    pub(crate) hard: Hard,
    pub(crate) entries: Vec<Entry>,
    pub(crate) applied: u64,
}

impl Disk {
    fn last(&self) -> Position {
        self.entries.last().map_or_else(Position::default, |e| e.at)
    }
}

pub(crate) struct Network {
    pub(crate) nodes: Vec<Raft>,
    pub(crate) disks: Vec<Disk>,
    // A message between a node that is cut off and one that is not is lost.
    pub(crate) cut: Vec<bool>,
    flight: Vec<Message>,
    leaders: BTreeMap<Term, node::Key>,
    // The last positions each candidate claimed, by candidate, term, and whether
    // the request was a PreVote.
    asked: BTreeMap<(node::Key, Term, bool), Vec<Position>>,
    /// Every entry some node applied, in index order.
    pub(crate) applied: Vec<Entry>,
    // The term of the leader that committed each entry of `applied`.
    committed: Vec<Term>,
    proposed: u16,
    // The state of a splitmix64 generator for the ticks of `round`.
    random: u64,
}

impl Network {
    pub(crate) fn new(logs: &[Position], random: u64) -> Self {
        // A term with entries had a leader that a quorum voted for, so no node
        // starts below the highest term in the logs.
        let term = logs.iter().map(|last| last.term).max().unwrap_or_default();
        let disks = logs
            .iter()
            .map(|last| Disk {
                hard: Hard { term, vote: None },
                entries: (1..=last.index)
                    .map(|index| Entry {
                        at: Position {
                            term: last.term,
                            index,
                        },
                        data: Data::Empty,
                    })
                    .collect(),
                applied: 0,
            })
            .collect();
        let mut network = Self {
            nodes: Vec::new(),
            disks,
            cut: vec![false; logs.len()],
            flight: Vec::new(),
            leaders: BTreeMap::new(),
            asked: BTreeMap::new(),
            applied: Vec::new(),
            committed: Vec::new(),
            proposed: 0,
            random,
        };
        for node in 0..logs.len() {
            let raft = network.build(node);
            network.nodes.push(raft);
        }
        network
    }

    pub(crate) fn key(node: usize) -> node::Key {
        node::Key::from_u128(node as u128 + 1)
    }

    fn at(&self, key: node::Key) -> usize {
        self.nodes
            .iter()
            .position(|node| node.key() == key)
            .unwrap()
    }

    fn build(&self, node: usize) -> Raft {
        let config = Config {
            key: Self::key(node),
            election_ticks: ELECTION,
            heartbeat_ticks: HEARTBEAT,
        };
        let disk = &self.disks[node];
        let start = Start {
            hard: disk.hard,
            voters: Voters {
                incoming: (0..self.disks.len()).map(Self::key).collect(),
                ..Voters::default()
            },
            entries: disk.entries.clone(),
            applied: disk.applied,
        };
        Raft::new(config, start).unwrap()
    }

    fn deliver(&mut self, message: Message) {
        let (from, to) = (self.at(message.from), self.at(message.to));
        if self.cut[from] == self.cut[to] {
            self.nodes[to].step(message).unwrap();
            self.collect();
        }
    }

    pub(crate) fn apply(&mut self, action: &Action) {
        match action {
            Action::Tick { node, random } => self.nodes[*node].tick(*random),
            Action::Campaign { node } => self.nodes[*node].campaign(),
            Action::Propose { node } => {
                self.propose(*node);
            }
            Action::Restart { node } => self.nodes[*node] = self.build(*node),
            Action::Deliver { picks } => {
                for pick in picks {
                    if self.flight.is_empty() {
                        break;
                    }
                    let at = pick.index(self.flight.len());
                    let message = self.flight.swap_remove(at);
                    self.deliver(message);
                }
            }
            Action::Repeat { pick } if !self.flight.is_empty() => {
                self.deliver(self.flight[pick.index(self.flight.len())].clone());
            }
            Action::Lose { pick } if !self.flight.is_empty() => {
                self.flight.swap_remove(pick.index(self.flight.len()));
            }
            Action::Repeat { .. } | Action::Lose { .. } => {}
            Action::Cut { node } => self.cut[*node] = true,
            Action::Mend => self.cut.fill(false),
            Action::ChangeVoters { node, members } => {
                self.change_voters(*node, members);
            }
        }
        self.collect();
    }

    // Proposes the voter set of the flagged nodes to `node` and checks the answer
    // against the node's state.
    fn change_voters(&mut self, node: usize, members: &[bool]) {
        let voters: BTreeSet<node::Key> = (0..members.len())
            .filter(|&at| members[at])
            .map(Self::key)
            .collect();
        let raft = &mut self.nodes[node];
        let result = raft.propose_voters(voters.clone());
        match (raft.role(), result) {
            (Role::Leader, Ok(at)) => assert_eq!(at.term, raft.term()),
            (role, Err(Error::NotLeader { leader })) if role != Role::Leader => {
                assert_eq!(leader, raft.leader());
            }
            (Role::Leader, Err(Error::NoVoters)) => assert!(voters.is_empty()),
            (Role::Leader, Err(Error::ChangePending { at })) => {
                let disk = &self.disks[node];
                let last = disk
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| matches!(entry.data, Data::Voters(_)));
                assert_eq!(last.map(|entry| entry.at), Some(at));
                assert!(at.index > disk.applied, "a committed change is pending");
            }
            (role, result) => panic!("a {role:?} answered a change with {result:?}"),
        }
    }

    /// Proposes a new value to `node`. Returns its position when the node leads.
    pub(crate) fn propose(&mut self, node: usize) -> Option<Position> {
        self.proposed += 1;
        let result = self.nodes[node].propose(self.proposed.to_le_bytes().to_vec());
        let raft = &self.nodes[node];
        let at = match (raft.role(), result) {
            (Role::Leader, Ok(at)) => {
                assert_eq!(at.term, raft.term());
                Some(at)
            }
            (role, Err(Error::NotLeader { leader })) if role != Role::Leader => {
                assert_eq!(leader, raft.leader());
                None
            }
            (role, result) => panic!("a {role:?} answered a proposal with {result:?}"),
        };
        self.collect();
        at
    }

    /// Takes each node's `Ready`, runs it against the node's disk, moves the
    /// messages into the network, and checks the safety properties.
    fn collect(&mut self) {
        for at in 0..self.nodes.len() {
            let ready = self.nodes[at].ready();
            if let Some(hard) = ready.hard {
                self.disks[at].hard = hard;
            }
            self.write(at, ready.entries);
            for message in ready.messages {
                self.note(at, &message);
                self.flight.push(message);
            }
            for entry in ready.committed {
                self.apply_entry(at, entry);
            }
            if self.nodes[at].role() == Role::Leader {
                self.check_leader(at);
            }
        }
    }

    // Writes entries as the `Ready` contract says: in place of any entry at or
    // after the first one's index.
    fn write(&mut self, at: usize, entries: Vec<Entry>) {
        let Some(first) = entries.first() else {
            return;
        };
        let disk = &mut self.disks[at];
        let from = usize::try_from(first.at.index - 1).unwrap();
        assert!(
            from <= disk.entries.len(),
            "a gap before the entries to write"
        );
        assert!(
            first.at.index > disk.applied,
            "a write below the applied index"
        );
        disk.entries.truncate(from);
        disk.entries.extend(entries);
        self.check_log_matching(at);
    }

    // Log matching: two logs that share one position agree on every entry up to it.
    fn check_log_matching(&self, at: usize) {
        let log = &self.disks[at].entries;
        for (other, disk) in self.disks.iter().enumerate() {
            if other == at {
                continue;
            }
            let common = log.len().min(disk.entries.len());
            let shared = (0..common).rev().find(|&i| log[i].at == disk.entries[i].at);
            if let Some(i) = shared {
                assert_eq!(log[..=i], disk.entries[..=i], "log matching");
            }
        }
    }

    // State machine safety: the applied entries of every node are one sequence.
    fn apply_entry(&mut self, at: usize, entry: Entry) {
        let disk = &mut self.disks[at];
        assert_eq!(entry.at.index, disk.applied + 1, "applied out of order");
        let index = usize::try_from(entry.at.index - 1).unwrap();
        assert_eq!(
            disk.entries.get(index),
            Some(&entry),
            "applied before written"
        );
        disk.applied = entry.at.index;
        if let Some(other) = self.applied.get(index) {
            assert_eq!(other, &entry, "state machine safety");
        } else {
            assert_eq!(self.applied.len(), index, "applied past the sequence");
            self.applied.push(entry);
            self.committed.push(self.nodes[at].term());
        }
    }

    // A vote goes only to a candidate whose log is at least as new as the voter's.
    fn note(&mut self, at: usize, message: &Message) {
        let prevote = match message.body {
            Body::PreVote { last } => {
                let key = (message.from, message.term, true);
                self.asked.entry(key).or_default().push(last);
                return;
            }
            Body::Vote { last } => {
                let key = (message.from, message.term, false);
                self.asked.entry(key).or_default().push(last);
                return;
            }
            Body::PreVoteReply { granted: true } => true,
            Body::VoteReply { granted: true } => false,
            _ => return,
        };
        let last = self.disks[at].last();
        let asked = &self.asked[&(message.to, message.term, prevote)];
        assert!(
            asked.iter().any(|&claimed| last <= claimed),
            "node {at} with log {last:?} granted a vote for {asked:?}"
        );
    }

    // Election safety and leader completeness, at the first sight of each leader.
    // A candidate can win a stale term after a later leader committed entries, so
    // a leader must hold only the entries committed in a term below its own.
    fn check_leader(&mut self, at: usize) {
        let node = &self.nodes[at];
        let term = node.term();
        if let Some(leader) = self.leaders.get(&term) {
            assert_eq!(*leader, node.key(), "two leaders in term {term:?}");
            return;
        }
        self.leaders.insert(term, node.key());
        let before = self.committed.iter().take_while(|&&c| c < term).count();
        assert!(
            self.disks[at].entries.starts_with(&self.applied[..before]),
            "leader completeness"
        );
    }

    /// Ticks each node once, then delivers every message in order, with no loss
    /// between nodes on the same side of the cut, until none remain.
    pub(crate) fn round(&mut self) {
        for node in &mut self.nodes {
            self.random = self.random.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.random;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            node.tick(z ^ (z >> 31));
        }
        self.collect();
        while !self.flight.is_empty() {
            for message in std::mem::take(&mut self.flight) {
                self.deliver(message);
            }
        }
    }

    /// The leader and its term, when one node leads and every other node in its
    /// configuration follows it in its term. A node a change removed hears nothing
    /// more from the leader, so its term and leader may lag for good.
    pub(crate) fn agreed(&self) -> Option<(usize, Term)> {
        let at = self
            .nodes
            .iter()
            .position(|node| node.role() == Role::Leader)?;
        let leader = &self.nodes[at];
        let agreed = self.voters(at).all(|node| {
            let node = &self.nodes[node];
            node.term() == leader.term() && node.leader() == Some(leader.key())
        });
        agreed.then_some((at, leader.term()))
    }

    /// The nodes in the configuration of `node`, in order.
    pub(crate) fn voters(&self, node: usize) -> impl Iterator<Item = usize> {
        let voters = self.nodes[node].voters();
        (0..self.nodes.len()).filter(move |&at| {
            let key = Self::key(at);
            voters.incoming.contains(&key) || voters.outgoing.contains(&key)
        })
    }

    /// Runs the actions, mends the network, and runs rounds until the nodes agree
    /// on one leader for two election timeouts.
    pub(crate) fn settle(
        &mut self,
        actions: &[Action],
    ) -> Result<(usize, Term), TestCaseError> {
        for action in actions {
            self.apply(action);
        }
        self.cut.fill(false);
        // A leader that was cut off can step down once after the network mends,
        // because it counts the nodes it heard from over a full election timeout.
        let mut held = (None, 0);
        for _ in 0..100 * ELECTION {
            self.round();
            let agreed = self.agreed();
            held = if agreed == held.0 {
                (agreed, held.1 + 1)
            } else {
                (agreed, 1)
            };
            if let (Some(agreed), true) = (held.0, held.1 >= 2 * ELECTION) {
                return Ok(agreed);
            }
        }
        Err(TestCaseError::fail("no leader after 100 election timeouts"))
    }
}
