//! The cost of a leader's heartbeat tick on a large log: the chain of each heartbeat
//! comes from the index of the configuration entries, not from a scan of the log. On a
//! long chain, each heartbeat clones only the links that fit and one more.

use std::collections::BTreeMap;

use divan::Bencher;
use raft::{
    Answer, Body, Change, Config, Data, Entry, Grant, Message, Position, Proof, Raft,
    Role, Start, Term, Voters,
};
use types::node;

const ENTRIES: [u64; 3] = [1_000, 100_000, 1_000_000];
const VOTERS: u128 = 7;
const LINKS: [u64; 2] = [300, 600];
// The links a heartbeat keeps under `kept`.
const KEPT: usize = 200;

fn main() {
    divan::main();
}

fn key(node: u128) -> node::Key {
    node::Key::from_u128(node)
}

fn voters() -> Voters {
    Voters {
        incoming: (1..=VOTERS).map(key).collect(),
        ..Voters::default()
    }
}

// Each entry of term 1 at `1..=count`, of `data`.
fn entries(count: u64, data: impl Fn() -> Data) -> Vec<Entry> {
    let entry = |index| Entry {
        at: Position {
            term: Term(1),
            index,
        },
        data: data(),
    };
    (1..=count).map(entry).collect()
}

// The configuration of `voters` that node 1 wrote with its own vote.
fn change() -> Data {
    Data::Voters(Change {
        voters: voters(),
        public_keys: BTreeMap::new(),
        votes: Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [(key(1), None)].into(),
        },
        signature: None,
    })
}

// Holds a message whose links name at most `KEPT` voter sets: a cost that grows with
// the chain, as the size of an encoded message does.
fn kept(message: &Message) -> bool {
    let sets = message
        .chain
        .iter()
        .map(|link| link.change.voters.incoming.len());
    sets.sum::<usize>() <= KEPT * VOTERS as usize
}

/// A leader of 7 voters, with `entries` in its log and no peer that answered it in
/// its term: each tick sends 6 heartbeats with a proof.
fn leader(fits: fn(&Message) -> bool, entries: Vec<Entry>) -> Raft {
    let config = Config {
        key: key(1),
        election_ticks: u32::MAX,
        heartbeat_ticks: 1,
        fits,
    };
    let start = Start {
        voters: voters(),
        entries,
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
    let mut raft = leader(|_| true, self::entries(entries, || Data::Empty));
    bencher.bench_local(|| {
        raft.tick(0);
        let ready = raft.ready();
        assert_eq!(ready.messages.len(), 6, "one heartbeat per silent peer");
        ready
    });
}

/// A tick of a leader whose log holds `links` configuration entries of term 1, and
/// whose `fits` keeps the newest `KEPT` of them.
#[divan::bench(args = LINKS)]
fn heartbeat_tick_long_chain(bencher: Bencher<'_, '_>, links: u64) {
    let mut raft = leader(kept, entries(links, change));
    bencher.bench_local(|| {
        raft.tick(0);
        let ready = raft.ready();
        let chains = ready.messages.iter().map(|message| message.chain.len());
        assert_eq!(
            chains.collect::<Vec<_>>(),
            [KEPT; 6],
            "each heartbeat keeps KEPT"
        );
        ready
    });
}
