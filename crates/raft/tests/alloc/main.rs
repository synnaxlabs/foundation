//! A chain of links of one term costs one copy of the trusted configuration, not
//! one per link, and a leader's heartbeat clones only the links that fit and one
//! more. This binary has no test harness: the count covers each thread, and a harness
//! allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::collections::BTreeMap;

use raft::{
    Answer, Body, Change, Config, Data, Entry, Grant, Link, Message, Position, Proof,
    Raft, Role, Start, Term, Voters,
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
    let (mut shorter, mut longer) = (leader(300), leader(600));
    let tick = |raft: &mut Raft| {
        raft.tick(0);
        raft.ready()
    };
    let (sent_shorter, with_shorter) = ALLOCATOR.count(|| tick(&mut shorter));
    let (sent_longer, with_longer) = ALLOCATOR.count(|| tick(&mut longer));
    for sent in [sent_shorter, sent_longer] {
        let sent = sent.messages.iter();
        let chains = sent.map(|message| (message.proof.is_some(), message.chain.len()));
        assert_eq!(
            chains.collect::<Vec<_>>(),
            [(true, 200); 2],
            "each heartbeat has a proof and 200 links"
        );
    }
    assert_eq!(
        with_shorter, with_longer,
        "a heartbeat clones only the links that fit and one more"
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
        fits: |_| true,
    };
    let start = Start {
        voters: voters(&[1, 2, 3]),
        ..Start::default()
    };
    Raft::new(config, start).expect("the start is valid")
}

/// Node 1 as the leader of `{1, 2, 3}` in term 3, with `links` configuration entries
/// of term 1 in its log and a `fits` that keeps the newest 200 links.
fn leader(links: u64) -> Raft {
    let config = Config {
        key: key(1),
        election_ticks: u32::MAX,
        heartbeat_ticks: 1,
        fits: |message| message.chain.len() <= 200,
    };
    let entry = |index| Entry {
        at: Position {
            term: Term(1),
            index,
        },
        data: Data::Voters(Change {
            voters: voters(&[1, 2, 3]),
            public_keys: BTreeMap::new(),
            votes: proof(1, &[1]),
            signature: None,
        }),
    };
    let start = Start {
        voters: voters(&[1, 2, 3]),
        entries: (1..=links).map(entry).collect(),
        ..Start::default()
    };
    let mut raft = Raft::new(config, start).expect("the start is valid");
    raft.campaign();
    // A pre-vote grant names the term the candidate asks for; a vote, its term.
    for (body, ahead) in [
        (
            Body::PreVoteReply {
                answer: Answer::Granted(None),
            },
            1,
        ),
        (
            Body::VoteReply {
                answer: Answer::Granted(None),
            },
            0,
        ),
    ] {
        let reply = Message {
            from: key(2),
            to: key(1),
            term: Term(raft.term().0 + ahead),
            body,
            proof: raft.hard().proof,
            chain: Vec::new(),
        };
        raft.step(reply).expect("a voter's grant is read");
    }
    assert_eq!(raft.role(), Role::Leader, "two grants of three elect it");
    drop(raft.ready());
    raft
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
                    public_keys: BTreeMap::new(),
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
