//! Election properties over a network that loses, delays, reorders, and repeats
//! messages, with nodes that restart from their stored state.

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::sample::Index;
use types::node;

use crate::{Config, Hard, Message, Position, Raft, Role, Start, Term};

const ELECTION: u32 = 3;

#[derive(Clone, Debug)]
enum Action {
    Tick { node: usize, random: u64 },
    Campaign { node: usize },
    Restart { node: usize },
    Deliver { pick: Index },
    Repeat { pick: Index },
    Lose { pick: Index },
}

fn action(nodes: usize) -> impl Strategy<Value = Action> {
    let node = 0..nodes;
    let pick = any::<Index>;
    prop_oneof![
        4 => (node.clone(), any::<u64>())
            .prop_map(|(node, random)| Action::Tick { node, random }),
        1 => node.clone().prop_map(|node| Action::Campaign { node }),
        1 => node.prop_map(|node| Action::Restart { node }),
        6 => pick().prop_map(|pick| Action::Deliver { pick }),
        1 => pick().prop_map(|pick| Action::Repeat { pick }),
        1 => pick().prop_map(|pick| Action::Lose { pick }),
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
    (1..=5usize).prop_flat_map(|nodes| {
        (
            prop::collection::vec(position(), nodes),
            prop::collection::vec(action(nodes), 0..400),
        )
    })
}

struct Mesh {
    logs: Vec<Position>,
    nodes: Vec<Raft>,
    flight: Vec<Message>,
    leaders: BTreeMap<Term, node::Key>,
}

impl Mesh {
    fn new(logs: Vec<Position>) -> Self {
        let mut mesh = Self {
            nodes: Vec::new(),
            flight: Vec::new(),
            leaders: BTreeMap::new(),
            logs,
        };
        for node in 0..mesh.logs.len() {
            let hard = Hard {
                term: mesh.logs[node].term,
                vote: None,
            };
            let raft = mesh.build(node, hard);
            mesh.nodes.push(raft);
        }
        mesh
    }

    fn key(node: usize) -> node::Key {
        node::Key::from_u128(node as u128 + 1)
    }

    fn build(&self, node: usize, hard: Hard) -> Raft {
        let config = Config {
            key: Self::key(node),
            election_ticks: ELECTION,
            heartbeat_ticks: 1,
        };
        let start = Start {
            hard,
            voters: (0..self.logs.len()).map(Self::key).collect(),
            last: self.logs[node],
        };
        Raft::new(config, start).unwrap()
    }

    fn deliver(&mut self, message: Message) {
        let node = self
            .nodes
            .iter_mut()
            .find(|node| node.key() == message.to)
            .unwrap();
        node.step(message).unwrap();
    }

    fn apply(&mut self, action: &Action) {
        match *action {
            Action::Tick { node, random } => self.nodes[node].tick(random),
            Action::Campaign { node } => self.nodes[node].campaign(),
            // Every message leaves a node after its hard state is stored, so a
            // restart keeps exactly the hard state.
            Action::Restart { node } => {
                self.nodes[node] = self.build(node, self.nodes[node].hard());
            }
            Action::Deliver { pick } if !self.flight.is_empty() => {
                let message = self.flight.swap_remove(pick.index(self.flight.len()));
                self.deliver(message);
            }
            Action::Repeat { pick } if !self.flight.is_empty() => {
                self.deliver(self.flight[pick.index(self.flight.len())]);
            }
            Action::Lose { pick } if !self.flight.is_empty() => {
                self.flight.swap_remove(pick.index(self.flight.len()));
            }
            Action::Deliver { .. } | Action::Repeat { .. } | Action::Lose { .. } => {}
        }
        self.collect();
    }

    /// Moves each node's messages into the network and records each leader.
    fn collect(&mut self) {
        for node in &mut self.nodes {
            self.flight.extend(node.messages());
            if node.role() == Role::Leader {
                let leader = *self.leaders.entry(node.term()).or_insert(node.key());
                assert_eq!(leader, node.key(), "two leaders in term {}", node.term());
            }
        }
    }

    /// Delivers every message in order, with no loss, until none remain.
    fn settle(&mut self) {
        while !self.flight.is_empty() {
            for message in std::mem::take(&mut self.flight) {
                self.deliver(message);
            }
            self.collect();
        }
    }

    /// The leader, when one node leads and every other node follows it in its term.
    fn agreed(&self) -> Option<node::Key> {
        let leader = self.nodes.iter().find(|node| node.role() == Role::Leader)?;
        let agreed = self.nodes.iter().all(|node| {
            node.term() == leader.term() && node.leader() == Some(leader.key())
        });
        agreed.then_some(leader.key())
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

proptest! {
    #[test]
    fn a_term_has_at_most_one_leader((logs, actions) in run()) {
        let mut mesh = Mesh::new(logs);
        for action in &actions {
            mesh.apply(action);
        }
    }

    #[test]
    fn a_healed_network_elects_one_leader(
        (logs, actions) in run(),
        mut seed in any::<u64>(),
    ) {
        let mut mesh = Mesh::new(logs);
        for action in &actions {
            mesh.apply(action);
        }
        let mut rounds = 0;
        while mesh.agreed().is_none() {
            rounds += 1;
            prop_assert!(rounds <= 100 * ELECTION, "no leader after {rounds} rounds");
            for node in 0..mesh.nodes.len() {
                mesh.nodes[node].tick(splitmix(&mut seed));
            }
            mesh.collect();
            mesh.settle();
        }
    }
}
