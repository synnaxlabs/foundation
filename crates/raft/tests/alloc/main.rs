//! A chain of links of one term costs one copy of the trusted configuration, not
//! one per link. This binary has no test harness: the count covers each thread, and
//! a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use raft::{
    Body, Change, Config, Grant, Link, Message, Position, Proof, Raft, Start, Term,
    Voters,
};
use types::node;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    let (mut short, mut long) = (node(), node());
    let (three, three_hundred) = (heartbeat(3), heartbeat(300));
    let (_, with_three) = ALLOCATOR.count(|| short.step(three));
    let (_, with_three_hundred) = ALLOCATOR.count(|| long.step(three_hundred));
    assert_eq!(
        (short.term(), long.term()),
        (TERM, TERM),
        "both chains prove the term"
    );
    assert_eq!(
        with_three, with_three_hundred,
        "a longer chain of one term costs no more copies"
    );
}

const TERM: Term = Term(5);

fn key(node: u128) -> node::Key {
    node::Key::from_u128(node)
}

fn voters(keys: &[u128]) -> Voters {
    Voters {
        incoming: keys.iter().copied().map(key).collect(),
        ..Voters::default()
    }
}

fn proof(candidate: u128, voters: &[u128]) -> Proof {
    Proof {
        grant: Grant::Vote,
        candidate: key(candidate),
        voters: voters.iter().map(|&voter| (key(voter), None)).collect(),
    }
}

/// Node 1 of the founding configuration `{1, 2, 3}`, with an empty log.
fn node() -> Raft {
    let config = Config {
        key: key(1),
        election_ticks: 10,
        heartbeat_ticks: 1,
    };
    let start = Start {
        voters: voters(&[1, 2, 3]),
        ..Start::default()
    };
    Raft::new(config, start).expect("the start is valid")
}

/// A heartbeat from node 9 for `TERM`, proven by its own grant through `links`
/// links of term 1. Each link is voted by a quorum of the founding configuration,
/// and only the last one makes node 9 a quorum alone.
fn heartbeat(links: u64) -> Message {
    let chain = (1..=links)
        .map(|index| {
            let last = index == links;
            Link {
                at: Position {
                    term: Term(1),
                    index,
                },
                change: Change {
                    voters: if last {
                        voters(&[9])
                    } else {
                        voters(&[1, 2, 3])
                    },
                    votes: proof(1, &[1, 2]),
                    signature: None,
                },
            }
        })
        .collect();
    Message {
        from: key(9),
        to: key(1),
        term: TERM,
        body: Body::Heartbeat { commit: 0 },
        proof: Some(proof(9, &[9])),
        chain,
    }
}
