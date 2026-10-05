//! A message is checked before it changes anything. One with any index or term a
//! peer could send never stops the node, and one the node refuses leaves it as it
//! was.

use proptest::prelude::*;
use proptest::sample::Index;
use raft::{Body, Data, Entry, Message, Position, Raft, Ready, Term, Voters};

use types::node;

use crate::network::{Action, Network, run};

const CASES: u32 = 2000;

// The indexes and terms a faulty peer is most likely to find a seam with.
fn edge() -> impl Strategy<Value = u64> {
    prop_oneof![0..4u64, Just(u64::MAX - 1), Just(u64::MAX)]
}

fn position() -> impl Strategy<Value = Position> {
    (edge(), edge()).prop_map(|(term, index)| Position {
        term: Term(term),
        index,
    })
}

fn entry() -> impl Strategy<Value = Entry> {
    let data = prop_oneof![
        Just(Data::Empty),
        Just(Data::Bytes(vec![7])),
        Just(Data::Voters(Voters::default())),
    ];
    (position(), data).prop_map(|(at, data)| Entry { at, data })
}

fn body() -> impl Strategy<Value = Body> {
    prop_oneof![
        position().prop_map(|last| Body::PreVote { last }),
        position().prop_map(|last| Body::Vote { last }),
        any::<bool>().prop_map(|granted| Body::PreVoteReply { granted }),
        any::<bool>().prop_map(|granted| Body::VoteReply { granted }),
        edge().prop_map(|commit| Body::Heartbeat { commit }),
        Just(Body::HeartbeatReply),
        (position(), prop::collection::vec(entry(), 0..3), edge()).prop_map(
            |(prev, entries, commit)| Body::Append {
                prev,
                entries,
                commit,
            }
        ),
        edge().prop_map(|last| Body::AppendReply { last }),
        edge().prop_map(|hint| Body::AppendReject { hint }),
    ]
}

// The node at `to` after `run`, its last log position, and the key of `from`,
// which ranges one past the nodes, so a stranger sends too.
fn receiver(
    (logs, actions): &(Vec<Position>, Vec<Action>),
    to: Index,
    from: Index,
) -> Result<(Raft, Position, node::Key), TestCaseError> {
    let mut network = Network::new(logs, 0);
    for action in actions {
        network.apply(action);
    }
    let nodes = network.nodes.len();
    let (to, from) = (to.index(nodes), from.index(nodes + 1));
    if from == to {
        return Err(TestCaseError::reject("a node does not send to itself"));
    }
    let mut raft = network.nodes.swap_remove(to);
    let pending = raft.ready().entries;
    let last = pending
        .last()
        .map_or_else(|| network.disks[to].last(), |entry| entry.at);
    Ok((raft, last, Network::key(from)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn a_refused_message_changes_nothing(
        run in run(),
        to in any::<Index>(),
        from in any::<Index>(),
        term in edge(),
        body in body(),
    ) {
        let (mut raft, _, from) = receiver(&run, to, from)?;
        let (hard, role, leader) = (raft.hard(), raft.role(), raft.leader());
        let message = Message { from, to: raft.key(), term: Term(term), body };
        if raft.step(message).is_err() {
            let after = (raft.hard(), raft.role(), raft.leader());
            prop_assert_eq!(after, (hard, role, leader));
            prop_assert_eq!(raft.ready(), Ready::default());
        }
    }

    // A stored term behind the log stops the node at its next start. The append
    // follows the node's log, and its entry terms rise from the last one.
    #[test]
    fn a_node_writes_no_entry_above_its_term(
        run in run(),
        to in any::<Index>(),
        from in any::<Index>(),
        ahead in 0..3u64,
        rises in prop::collection::vec(0..3u64, 1..4),
    ) {
        let (mut raft, prev, from) = receiver(&run, to, from)?;
        let mut at = prev;
        let entries = rises
            .iter()
            .map(|rise| {
                at = Position {
                    term: Term(at.term.0.saturating_add(*rise).max(1)),
                    index: at.index.saturating_add(1),
                };
                Entry { at, data: Data::Empty }
            })
            .collect();
        let body = Body::Append { prev, entries, commit: 0 };
        let term = Term(raft.term().0.saturating_add(ahead));
        let message = Message { from, to: raft.key(), term, body };
        if raft.step(message).is_ok() {
            let term = raft.hard().term;
            for entry in raft.ready().entries {
                prop_assert!(entry.at.term <= term, "{entry:?} above term {term:?}");
            }
        }
    }
}
