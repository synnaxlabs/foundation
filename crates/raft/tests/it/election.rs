//! Election properties over a network that cuts nodes off and loses, delays, reorders,
//! and repeats messages, with nodes that restart from their stored state.

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::sample::Index;
use raft::{Config, Hard, Message, Position, Raft, Role, Start, Term};
use types::node;

const ELECTION: u32 = 5;
const HEARTBEAT: u32 = 2;

#[derive(Clone, Debug)]
enum Action {
    Tick { node: usize, random: u64 },
    Campaign { node: usize },
    Restart { node: usize },
    Deliver { picks: Vec<Index> },
    Repeat { pick: Index },
    Lose { pick: Index },
    Cut { node: usize },
    Mend,
}

fn action(nodes: usize) -> impl Strategy<Value = Action> {
    let node = 0..nodes;
    let pick = any::<Index>;
    prop_oneof![
        4 => (node.clone(), any::<u64>())
            .prop_map(|(node, random)| Action::Tick { node, random }),
        1 => node.clone().prop_map(|node| Action::Campaign { node }),
        1 => node.clone().prop_map(|node| Action::Restart { node }),
        6 => prop::collection::vec(pick(), 1..8)
            .prop_map(|picks| Action::Deliver { picks }),
        1 => pick().prop_map(|pick| Action::Repeat { pick }),
        1 => pick().prop_map(|pick| Action::Lose { pick }),
        1 => node.prop_map(|node| Action::Cut { node }),
        1 => Just(Action::Mend),
    ]
}

fn position() -> impl Strategy<Value = Position> {
    (0..3u64, 0..3u64).prop_map(|(term, index)| Position {
        term: Term(term),
        index,
    })
}

/// The last log position of each node, and the actions to run.
fn run() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    let nodes = prop_oneof![1 => 1..=2_usize, 5 => Just(3), 1 => Just(4), 4 => Just(5)];
    run_of(nodes)
}

/// A run of a group that keeps a quorum when one node is cut off.
fn run_of_many() -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    run_of(prop_oneof![5 => Just(3_usize), 1 => Just(4), 4 => Just(5)])
}

fn run_of(
    nodes: impl Strategy<Value = usize>,
) -> impl Strategy<Value = (Vec<Position>, Vec<Action>)> {
    nodes.prop_flat_map(|nodes| {
        (
            prop::collection::vec(position(), nodes),
            prop::collection::vec(action(nodes), 0..400),
        )
    })
}

struct Network {
    logs: Vec<Position>,
    nodes: Vec<Raft>,
    // A message between a node that is cut off and one that is not is lost.
    cut: Vec<bool>,
    flight: Vec<Message>,
    leaders: BTreeMap<Term, node::Key>,
    // The state of a splitmix64 generator for the ticks of `round`.
    random: u64,
}

impl Network {
    fn new(logs: Vec<Position>, random: u64) -> Self {
        let mut network = Self {
            nodes: Vec::new(),
            cut: vec![false; logs.len()],
            flight: Vec::new(),
            leaders: BTreeMap::new(),
            logs,
            random,
        };
        for node in 0..network.logs.len() {
            let hard = Hard {
                term: network.logs[node].term,
                vote: None,
            };
            let raft = network.build(node, hard);
            network.nodes.push(raft);
        }
        network
    }

    fn key(node: usize) -> node::Key {
        node::Key::from_u128(node as u128 + 1)
    }

    fn at(&self, key: node::Key) -> usize {
        self.nodes
            .iter()
            .position(|node| node.key() == key)
            .unwrap()
    }

    fn build(&self, node: usize, hard: Hard) -> Raft {
        let config = Config {
            key: Self::key(node),
            election_ticks: ELECTION,
            heartbeat_ticks: HEARTBEAT,
        };
        let start = Start {
            hard,
            voters: (0..self.logs.len()).map(Self::key).collect(),
            last: self.logs[node],
        };
        Raft::new(config, start).unwrap()
    }

    fn deliver(&mut self, message: Message) {
        let (from, to) = (self.at(message.from), self.at(message.to));
        if self.cut[from] == self.cut[to] {
            self.nodes[to].step(message).unwrap();
        }
    }

    fn apply(&mut self, action: &Action) {
        match action {
            Action::Tick { node, random } => self.nodes[*node].tick(*random),
            Action::Campaign { node } => self.nodes[*node].campaign(),
            // Every message leaves a node after its hard state is stored, so a
            // restart keeps exactly the hard state.
            Action::Restart { node } => {
                self.nodes[*node] = self.build(*node, self.nodes[*node].hard());
            }
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
                self.deliver(self.flight[pick.index(self.flight.len())]);
            }
            Action::Lose { pick } if !self.flight.is_empty() => {
                self.flight.swap_remove(pick.index(self.flight.len()));
            }
            Action::Repeat { .. } | Action::Lose { .. } => {}
            Action::Cut { node } => self.cut[*node] = true,
            Action::Mend => self.cut.fill(false),
        }
        self.collect();
    }

    /// Moves each node's messages into the network and checks each leader.
    fn collect(&mut self) {
        for (at, node) in self.nodes.iter_mut().enumerate() {
            self.flight.extend(node.messages());
            if node.role() == Role::Leader {
                let leader = *self.leaders.entry(node.term()).or_insert(node.key());
                assert_eq!(leader, node.key(), "two leaders in term {}", node.term());
                let log = self.logs[at];
                let behind = self.logs.iter().filter(|other| **other <= log).count();
                assert!(behind > self.logs.len() / 2, "a leader behind a quorum");
            }
        }
    }

    /// Ticks each node once, then delivers every message in order, with no loss
    /// between nodes on the same side of the cut, until none remain.
    fn round(&mut self) {
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
            self.collect();
        }
    }

    /// The leader and its term, when one node leads and every other node follows it
    /// in its term.
    fn agreed(&self) -> Option<(usize, Term)> {
        let at = self
            .nodes
            .iter()
            .position(|node| node.role() == Role::Leader)?;
        let leader = &self.nodes[at];
        let agreed = self.nodes.iter().all(|node| {
            node.term() == leader.term() && node.leader() == Some(leader.key())
        });
        agreed.then_some((at, leader.term()))
    }

    /// Runs the actions, mends the network, and runs rounds until the nodes agree
    /// on one leader for two election timeouts.
    fn settle(&mut self, actions: &[Action]) -> Result<(usize, Term), TestCaseError> {
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn a_term_has_one_leader_with_a_log_no_older_than_a_quorum(
        (logs, actions) in run(),
    ) {
        let mut network = Network::new(logs, 0);
        for action in &actions {
            network.apply(action);
        }
    }

    #[test]
    fn a_mended_network_elects_one_leader_and_keeps_it(
        (logs, actions) in run(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(logs, random);
        let agreed = network.settle(&actions)?;
        for _ in 0..4 * ELECTION {
            network.round();
            prop_assert_eq!(network.agreed(), Some(agreed));
        }
    }

    #[test]
    fn a_node_that_was_cut_off_does_not_replace_the_leader(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
        pick in any::<Index>(),
    ) {
        let mut network = Network::new(logs, random);
        let agreed = network.settle(&actions)?;
        let others = network.nodes.len() - 1;
        let follower = (agreed.0 + 1 + pick.index(others)) % network.nodes.len();
        network.cut[follower] = true;
        for _ in 0..4 * ELECTION {
            network.round();
        }
        network.cut[follower] = false;
        for _ in 0..4 * ELECTION {
            network.round();
        }
        prop_assert_eq!(network.agreed(), Some(agreed));
    }

    #[test]
    fn a_leader_without_a_quorum_steps_down_and_the_rest_elect_another(
        (logs, actions) in run_of_many(),
        random in any::<u64>(),
    ) {
        let mut network = Network::new(logs, random);
        let (leader, _) = network.settle(&actions)?;
        network.cut[leader] = true;
        for _ in 0..2 * ELECTION {
            network.round();
        }
        prop_assert_ne!(network.nodes[leader].role(), Role::Leader);
        // `collect` checks the log of the new leader against a quorum.
        for _ in 0..20 * ELECTION {
            network.round();
        }
        let elected = network.nodes.iter().enumerate();
        let elected = elected.filter(|(_, node)| node.role() == Role::Leader);
        let elected: Vec<usize> = elected.map(|(at, _)| at).collect();
        prop_assert!(elected.len() == 1 && elected[0] != leader, "{elected:?}");
    }
}

// Enough cases that each election-safety change tried in review fails a run.
const CASES: u32 = 2000;
