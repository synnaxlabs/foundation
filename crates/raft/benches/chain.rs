//! The cost of a leader's heartbeat tick on a large log: the chain of each heartbeat
//! comes from the index of the configuration entries, not from a scan of the log.

use divan::Bencher;
use raft::{
    Answer, Body, Config, Data, Entry, Message, Position, Raft, Role, Start, Term,
    Voters,
};
use types::node;

const ENTRIES: [u64; 3] = [1_000, 100_000, 1_000_000];
const VOTERS: u128 = 7;

fn main() {
    divan::main();
}

fn key(node: u128) -> node::Key {
    node::Key::from_u128(node)
}

/// A leader of 7 voters, with `entries` entries of term 1 in its log and no peer
/// that answered it in its term: each tick sends 6 heartbeats with a proof.
fn leader(entries: u64) -> Raft {
    let config = Config {
        key: key(1),
        election_ticks: u32::MAX,
        heartbeat_ticks: 1,
    };
    let start = Start {
        voters: Voters {
            incoming: (1..=VOTERS).map(key).collect(),
            ..Voters::default()
        },
        entries: (1..=entries)
            .map(|index| Entry {
                at: Position {
                    term: Term(1),
                    index,
                },
                data: Data::Empty,
            })
            .collect(),
        ..Start::default()
    };
    let mut raft = Raft::new(config, start).expect("the start is valid");
    raft.campaign();
    let granted = || Answer::Granted(None);
    // A pre-vote grant names the term the candidate asks for; a vote, its term.
    for (body, ahead) in [
        (Body::PreVoteReply { answer: granted() }, 1),
        (Body::VoteReply { answer: granted() }, 0),
    ] {
        let (term, proof) = (Term(raft.term().0 + ahead), raft.hard().proof);
        for voter in 2..=4 {
            let reply = Message {
                from: key(voter),
                to: key(1),
                term,
                body: body.clone(),
                proof: proof.clone(),
                chain: Vec::new(),
            };
            raft.step(reply).expect("a voter's grant is read");
        }
    }
    assert_eq!(raft.role(), Role::Leader, "four grants of seven elect it");
    drop(raft.ready());
    raft
}

#[divan::bench(args = ENTRIES)]
fn heartbeat_tick(bencher: Bencher<'_, '_>, entries: u64) {
    let mut raft = leader(entries);
    bencher.bench_local(|| {
        raft.tick(0);
        let ready = raft.ready();
        assert_eq!(ready.messages.len(), 6, "one heartbeat per silent peer");
        ready
    });
}
