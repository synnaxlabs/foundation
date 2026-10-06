//! A network of nodes that cuts nodes off and loses, delays, reorders, and repeats
//! messages, with nodes that restart from a modeled disk, with or without their last
//! write. It checks the safety properties of Raft after every input.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::sample::Index;
use proptest::strategy::Union;
use raft::{
    Body, Config, Data, Entry, Error, Grant, Hard, Message, Position, Raft, Role,
    Start, Term, Voters,
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
    // The node crashes at its next `Ready` with something to write, after it wrote
    // only `kept` of the two.
    Crash { node: usize, kept: Kept },
    Deliver { picks: Vec<Index> },
    Repeat { pick: Index },
    Lose { pick: Index },
    Cut { node: usize },
    Mend,
    // A proposal to `node` of the flagged nodes as the voters.
    ChangeVoters { node: usize, voters: Vec<bool> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kept {
    Hard,
    Entries,
}

// The actions of a group of `nodes`. With `fixed`, the voters do not change.
fn action(nodes: usize, fixed: bool) -> impl Strategy<Value = Action> {
    let node = 0..nodes;
    let pick = any::<Index>;
    let mut arms: Vec<(u32, BoxedStrategy<Action>)> = vec![
        (
            4,
            (node.clone(), any::<u64>())
                .prop_map(|(node, random)| Action::Tick { node, random })
                .boxed(),
        ),
        (
            1,
            node.clone()
                .prop_map(|node| Action::Campaign { node })
                .boxed(),
        ),
        (
            3,
            node.clone()
                .prop_map(|node| Action::Propose { node })
                .boxed(),
        ),
        (
            1,
            node.clone()
                .prop_map(|node| Action::Restart { node })
                .boxed(),
        ),
        (
            1,
            (
                node.clone(),
                prop_oneof![Just(Kept::Hard), Just(Kept::Entries)],
            )
                .prop_map(|(node, kept)| Action::Crash { node, kept })
                .boxed(),
        ),
        (
            6,
            prop::collection::vec(pick(), 1..8)
                .prop_map(|picks| Action::Deliver { picks })
                .boxed(),
        ),
        (1, pick().prop_map(|pick| Action::Repeat { pick }).boxed()),
        (1, pick().prop_map(|pick| Action::Lose { pick }).boxed()),
        (
            1,
            node.clone().prop_map(|node| Action::Cut { node }).boxed(),
        ),
        (1, Just(Action::Mend).boxed()),
    ];
    if !fixed {
        let change = (node, prop::collection::vec(any::<bool>(), nodes))
            .prop_map(|(node, voters)| Action::ChangeVoters { node, voters });
        arms.push((1, change.boxed()));
    }
    Union::new_weighted(arms)
}

// An empty log ends at the zero position; any other ends in a term above zero.
fn position() -> impl Strategy<Value = Position> {
    let filled = (1..3u64, 1..3u64).prop_map(|(term, index)| Position {
        term: Term(term),
        index,
    });
    prop_oneof![1 => Just(Position::default()), 3 => filled]
}

/// The last log position of each node, and the actions to run, with changes to the
/// voters.
pub(crate) fn run() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    let nodes = prop_oneof![1 => 1..=2_usize, 5 => Just(3), 1 => Just(4), 4 => Just(5)];
    run_of(nodes, false)
}

/// A run of a group that keeps a quorum when one node is cut off. The voters do not
/// change.
pub(crate) fn run_of_many() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    run_of(
        prop_oneof![5 => Just(3_usize), 1 => Just(4), 4 => Just(5)],
        true,
    )
}

fn run_of(
    nodes: impl Strategy<Value = usize>,
    fixed: bool,
) -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    nodes.prop_flat_map(move |nodes| {
        (
            prop::collection::vec(position(), nodes),
            prop::collection::vec(action(nodes, fixed), 0..400),
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
    pub(crate) fn last(&self) -> Position {
        self.entries.last().map_or_else(Position::default, |e| e.at)
    }
}

pub(crate) struct Network {
    pub(crate) nodes: Vec<Raft>,
    pub(crate) disks: Vec<Disk>,
    // A message between a node that is cut off and one that is not is lost.
    pub(crate) cut: Vec<bool>,
    // A node whose disk lost entries it synced. Its `IndexPastLog` errors go to
    // `refused`; any other error fails the run.
    wiped: Vec<bool>,
    pub(crate) refused: Vec<Error>,
    // A node refused a proof that fits its body but is no quorum of its
    // configuration: it is behind a configuration change. The known gap.
    behind_refused: bool,
    crash: Vec<Option<Kept>>,
    flight: Vec<Message>,
    leaders: BTreeMap<Term, node::Key>,
    // The last positions each candidate claimed, by candidate, term, and whether
    // the request was a PreVote.
    asked: BTreeMap<(node::Key, Term, bool), Vec<Position>>,
    // The voters whose grant reached each candidate, forged ones included, by
    // candidate, term, and whether the grant was a PreVote.
    granted: BTreeMap<(node::Key, Term, bool), BTreeSet<node::Key>>,
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
                hard: Hard {
                    term,
                    vote: None,
                    leader: None,
                    proof: None,
                },
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
            wiped: vec![false; logs.len()],
            refused: Vec::new(),
            behind_refused: false,
            crash: vec![None; logs.len()],
            flight: Vec::new(),
            leaders: BTreeMap::new(),
            asked: BTreeMap::new(),
            granted: BTreeMap::new(),
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
            hard: disk.hard.clone(),
            voters: self.base(),
            entries: disk.entries.clone(),
            applied: disk.applied,
        };
        Raft::new(config, start).unwrap()
    }

    // The configuration every node starts from: every node.
    fn base(&self) -> Voters {
        Voters {
            incoming: (0..self.disks.len()).map(Self::key).collect(),
            ..Voters::default()
        }
    }

    // The configuration `raft` counts as committed: the last configuration entry the
    // node committed, else the base. `applied` is the node's commit index after each
    // `collect`.
    fn committed_voters(&self, node: usize) -> Voters {
        let disk = &self.disks[node];
        let committed = disk
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.at.index <= disk.applied)
            .find_map(|entry| match &entry.data {
                Data::Voters(voters) => Some(voters),
                Data::Empty | Data::Bytes(_) => None,
            });
        committed.map_or_else(|| self.base(), Voters::clone)
    }

    // Whether `voters` is a quorum of the configuration of `node`, in force or last
    // committed: a majority of each set, as `raft` counts it.
    fn quorum(&self, node: usize, voters: &BTreeSet<node::Key>) -> bool {
        let majority = |set: &BTreeSet<node::Key>| {
            set.is_empty() || 2 * set.intersection(voters).count() > set.len()
        };
        let quorum =
            |config: &Voters| majority(&config.incoming) && majority(&config.outgoing);
        quorum(self.nodes[node].voters()) || quorum(&self.committed_voters(node))
    }

    /// Whether `node` is behind `leader`: no quorum of the configuration `node`
    /// holds voted for it, so `node` refuses the leader until an election it can
    /// prove. A known gap: the leader does not yet prove the change that removed the
    /// voters `node` still counts.
    pub(crate) fn behind(&self, node: usize, leader: usize) -> bool {
        let key = Self::key(leader);
        let term = self.nodes[leader].term();
        let mut votes = self
            .granted
            .get(&(key, term, false))
            .cloned()
            .unwrap_or_default();
        votes.insert(key);
        !self.quorum(node, &votes)
    }

    pub(crate) fn deliver(&mut self, message: &Message) {
        let (from, to) = (self.at(message.from), self.at(message.to));
        if self.cut[from] == self.cut[to] {
            let prevote = match message.body {
                Body::PreVoteReply { granted: true } => Some(true),
                Body::VoteReply { granted: true } => Some(false),
                _ => None,
            };
            if let Some(prevote) = prevote {
                let key = (message.to, message.term, prevote);
                self.granted.entry(key).or_default().insert(message.from);
            }
            let before = self.nodes[to].term();
            match self.nodes[to].step(message.clone()) {
                Err(error @ Error::IndexPastLog { .. }) if self.wiped[to] => {
                    self.refused.push(error);
                }
                Err(Error::Unproven { .. }) => {
                    self.check_unproven(to, before, message);
                }
                result => result.unwrap(),
            }
            self.collect();
        }
    }

    // A node refuses as unproven only a message whose proof is missing, does not fit
    // its body, or is no quorum of its configuration, and it stays in its term. In
    // its own term, only a leader needs a proof, and only while it knows none.
    fn check_unproven(&mut self, to: usize, before: Term, message: &Message) {
        let (body, term, from) = (&message.body, message.term, message.from);
        assert_eq!(
            self.nodes[to].term(),
            before,
            "node {to} moved on a refusal"
        );
        let grant = match (body, term == before) {
            (Body::Heartbeat { .. } | Body::Append { .. }, same) => {
                assert!(
                    !same || self.disks[to].hard.leader.is_none(),
                    "node {to} refuses a leader in a term whose leader it knows"
                );
                Some(Grant::Vote)
            }
            (Body::Vote { .. }, false) => Some(Grant::PreVote),
            (
                Body::PreVoteReply { granted: false }
                | Body::VoteReply { .. }
                | Body::HeartbeatReply
                | Body::AppendReply { .. }
                | Body::AppendReject { .. },
                false,
            ) => None,
            _ => panic!("node {to} in {before:?} refuses {body:?} at {term:?}"),
        };
        let Some(proof) = &message.proof else {
            return;
        };
        let fits =
            grant.is_none_or(|grant| proof.grant == grant && proof.candidate == from);
        assert!(
            fits,
            "node {to} refuses a proof that fits {body:?}: {proof:?}"
        );
        assert!(
            !self.quorum(to, &proof.voters),
            "node {to} refuses a proven {body:?} at {term:?} from {from:?}"
        );
        self.behind_refused = true;
    }

    /// Restarts `node` from a disk that lost each entry after the first `keep`, and
    /// what it applied of them.
    pub(crate) fn wipe(&mut self, node: usize, keep: usize) {
        let disk = &mut self.disks[node];
        disk.entries.truncate(keep);
        disk.applied = disk.applied.min(disk.last().index);
        self.wiped[node] = true;
        self.nodes[node] = self.build(node);
    }

    pub(crate) fn apply(&mut self, action: &Action) {
        match action {
            Action::Tick { node, random } => self.nodes[*node].tick(*random),
            Action::Campaign { node } => self.nodes[*node].campaign(),
            Action::Propose { node } => {
                self.propose(*node);
            }
            Action::Restart { node } => self.nodes[*node] = self.build(*node),
            Action::Crash { node, kept } => self.crash[*node] = Some(*kept),
            Action::Deliver { picks } => {
                for pick in picks {
                    if self.flight.is_empty() {
                        break;
                    }
                    let at = pick.index(self.flight.len());
                    let message = self.flight.swap_remove(at);
                    self.deliver(&message);
                }
            }
            Action::Repeat { pick } if !self.flight.is_empty() => {
                let message = self.flight[pick.index(self.flight.len())].clone();
                self.deliver(&message);
            }
            Action::Lose { pick } if !self.flight.is_empty() => {
                self.flight.swap_remove(pick.index(self.flight.len()));
            }
            Action::Repeat { .. } | Action::Lose { .. } => {}
            Action::Cut { node } => self.cut[*node] = true,
            Action::Mend => self.cut.fill(false),
            Action::ChangeVoters { node, voters } => {
                self.change_voters(*node, voters);
            }
        }
        self.collect();
    }

    fn change_voters(&mut self, node: usize, flags: &[bool]) {
        let voters: BTreeSet<node::Key> = (0..flags.len())
            .filter(|&at| flags[at])
            .map(Self::key)
            .collect();
        let pending = self.pending(node);
        let raft = &mut self.nodes[node];
        let before = raft.voters().clone();
        let result = raft.propose_voters(voters.clone());
        let (role, term) = (raft.role(), raft.term());
        match (role, result) {
            (Role::Leader, Ok(at)) => {
                assert!(!voters.is_empty());
                assert_eq!(pending, None);
                assert!(
                    before.outgoing.is_empty(),
                    "a leader in a settled joint phase"
                );
                assert_eq!(at.term, term);
                let disk = &self.disks[node];
                assert_eq!(at.index, u64::try_from(disk.entries.len()).unwrap() + 1);
                let lost = self.crash[node] == Some(Kept::Hard);
                self.collect();
                let joint = Voters {
                    incoming: voters,
                    outgoing: before.incoming,
                };
                let index = usize::try_from(at.index - 1).unwrap();
                let written = self.disks[node].entries.get(index).map(|e| &e.data);
                let joint = Data::Voters(joint);
                assert_eq!(written, (!lost).then_some(&joint), "the joint entry");
            }
            (role, Err(Error::NotLeader { leader })) if role != Role::Leader => {
                assert_eq!(leader, self.nodes[node].leader());
            }
            (Role::Leader, Err(Error::NoVoters)) => assert!(voters.is_empty()),
            (Role::Leader, Err(Error::ChangePending { at })) => {
                assert_eq!(pending, Some(at));
            }
            (role, result) => panic!("a {role:?} answered a change with {result:?}"),
        }
    }

    // The position of the last configuration entry on the disk of `node` when it is
    // not committed.
    fn pending(&self, node: usize) -> Option<Position> {
        let disk = &self.disks[node];
        disk.entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.data, Data::Voters(_)))
            .filter(|entry| entry.at.index > disk.applied)
            .map(|entry| entry.at)
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
            let pending = ready.hard.is_some() || !ready.entries.is_empty();
            let kept = self.crash[at].take_if(|_| pending);
            if let Some(hard) = ready.hard
                && kept != Some(Kept::Entries)
            {
                self.disks[at].hard = hard;
            }
            if kept != Some(Kept::Hard) {
                self.write(at, ready.entries);
            }
            if kept.is_some() {
                self.nodes[at] = self.build(at);
                continue;
            }
            // A node sends nothing of a term before its hard state of that term is
            // durable. A prevote is the exception: it takes no term.
            let stored = self.disks[at].hard.term;
            for message in ready.messages {
                let durable = match message.body {
                    Body::PreVote { .. } | Body::PreVoteReply { granted: true } => true,
                    _ => message.term <= stored,
                };
                assert!(
                    durable,
                    "node {at} sends {:?} at {:?} above its stored {stored:?}",
                    message.body, message.term
                );
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
    // A proof names the sender, with the grant its body carries, and only the voters
    // that granted it.
    fn note(&mut self, at: usize, message: &Message) {
        if let Some(proof) = &message.proof {
            let grant = match message.body {
                Body::Vote { .. } => Some(Grant::PreVote),
                Body::Heartbeat { .. } | Body::Append { .. } => Some(Grant::Vote),
                _ => None,
            };
            if let Some(grant) = grant {
                let key = (message.from, message.term, grant == Grant::PreVote);
                assert_eq!(proof.grant, grant, "node {at} carries the wrong grant");
                assert_eq!(proof.candidate, message.from, "node {at} proves another");
                let mut granted = self.granted.get(&key).cloned().unwrap_or_default();
                granted.insert(message.from);
                assert!(
                    proof.voters.is_subset(&granted),
                    "node {at} carries voters that did not grant: {:?}",
                    proof.voters.difference(&granted).collect::<Vec<_>>()
                );
            }
        }
        if matches!(message.body, Body::PreVote { .. } | Body::Vote { .. }) {
            let node = &self.nodes[at];
            let key = node.key();
            let voter = |voters: &Voters| {
                voters.incoming.contains(&key) || voters.outgoing.contains(&key)
            };
            // Every configuration entry before the last is committed, so the one
            // before a pending entry is the committed one.
            assert!(
                voter(node.voters())
                    || (self.pending(at).is_some()
                        && voter(&self.committed_voters(at))),
                "node {at} campaigns outside its configuration"
            );
        }
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
        assert!(
            node.voters().incoming.contains(&node.key()) || self.pending(at).is_some(),
            "node {at} leads outside its committed configuration"
        );
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
                self.deliver(&message);
            }
        }
    }

    /// The leader and its term, when one node leads and every other node in its
    /// configuration, except one `behind`, follows it in its term. A node that a
    /// change removed gets no more messages from the leader, so its term and leader
    /// can lag.
    pub(crate) fn agreed(&self) -> Option<(usize, Term)> {
        let at = self
            .nodes
            .iter()
            .position(|node| node.role() == Role::Leader)?;
        let leader = &self.nodes[at];
        let agreed =
            self.voters(at)
                .filter(|&node| !self.behind(node, at))
                .all(|node| {
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

    /// Runs the actions, mends the network, and runs rounds until the leader's
    /// voters agree on it for two election timeouts.
    pub(crate) fn settle(
        &mut self,
        actions: &[Action],
    ) -> Result<(usize, Term), TestCaseError> {
        for action in actions {
            self.apply(action);
        }
        self.cut.fill(false);
        self.crash.fill(None);
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
        if self.behind_refused {
            return Err(TestCaseError::reject(
                "no leader: a voter behind a configuration change refused the group",
            ));
        }
        Err(TestCaseError::fail("no leader after 100 election timeouts"))
    }

    /// Runs rounds, up to four election timeouts, until every one of `nodes` has
    /// applied `index`.
    pub(crate) fn apply_until(&mut self, nodes: &[usize], index: u64) {
        for _ in 0..4 * ELECTION {
            if nodes.iter().all(|&node| self.disks[node].applied >= index) {
                return;
            }
            self.round();
        }
    }
}
