//! Test helpers that the modules of `mesh` reuse: keys, signed messages, and a pool.
//! Node `id` has the private key `[id; 32]`.

use std::rc::Rc;

use block::Pool;
use raft::{
    Answer, Body, Data, Entry, Grant, Message, Position, Proof, Ready, Signature, Term,
    Voters,
};
use transport::Address;
use types::channel;
use types::digest::Digest;
use types::ed25519::PublicKey;
use types::name::Name;
use types::node::{self, PrivateKey, SealKey};
use types::time::{Span, Stamp};

use crate::bytes::{put_channel, put_count, put_name};
use crate::card::{self, Card};
use crate::change::{Change, Join};
use crate::claim::Signer;
use crate::member::Member;
use crate::pointer::Pointer;
use crate::status::Status;
use crate::ticket::{Options, Ticket, Voter};

/// The term of each message.
pub(crate) const TERM: Term = Term(5);

pub(crate) const EXPIRY: Stamp = Stamp::from_nanos(1_000);
const BEFORE_EXPIRY: Stamp = Stamp::from_nanos(999);
pub(crate) const EPHEMERAL: Span = Span::from_nanos(60);

pub(crate) fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

pub(crate) fn private(id: u8) -> PrivateKey {
    PrivateKey([id; 32])
}

pub(crate) fn signer(id: u8) -> Signer {
    Signer::new(key(id), &private(id))
}

pub(crate) fn public(id: u8) -> PublicKey {
    private(id).public()
}

/// The card of node `id` with `name`, which the node signed.
pub(crate) fn signed(id: u8, name: &str) -> card::Signed {
    let card = Card {
        name: name.parse().unwrap(),
        public_key: public(id),
        seal_key: SealKey::new([9; 32]).unwrap(),
        addresses: card::addresses::Addresses::new(Vec::new()).unwrap(),
        version: 1,
    };
    card::Signed::sign(key(id), card, &private(id))
}

/// Voter 1 at `10.0.0.1:4000`.
pub(crate) fn voter() -> Voter {
    let address = Address::Udp("10.0.0.1:4000".parse().unwrap());
    Voter {
        key: key(1),
        public_key: public(1),
        addresses: card::addresses::Addresses::new(vec![address]).unwrap(),
    }
}

/// The ticket of region `plant` with the private key of node `id`.
pub(crate) fn ticket(id: u8) -> Ticket {
    Ticket::new(private(id), "plant".parse().unwrap(), vec![voter()])
}

/// The record of node `id`, with a card that the node signed.
pub(crate) fn member(id: u8) -> Member {
    Member {
        card: signed(id, &format!("plant.node{id}")),
        admission: [0; 64],
        ephemeral: None,
        status: status([]),
    }
}

/// The byte form of a status of `count` entries, named `s00` on, with keys from 0 on.
/// It can hold more than a `Status` can.
pub(crate) fn status_bytes(count: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_count(usize::try_from(count).unwrap(), &mut out);
    for i in 0..count {
        put_name(&format!("s{i:02}").parse().unwrap(), &mut out);
        put_channel(channel::Key::from_u128(i), &mut out);
    }
    out
}

/// A status of the `entries`, at most 64.
pub(crate) fn status<const N: usize>(entries: [(Name, channel::Key); N]) -> Status {
    Status::new(entries.into()).unwrap()
}

pub(crate) fn create_members(ids: &[u8]) -> Vec<Member> {
    ids.iter().map(|&id| member(id)).collect()
}

/// A pool of 1 MiB. It reserves 51 MiB, so the tests of 8 threads stay under the
/// 4 GiB that CI gives a test process.
pub(crate) fn create_pool() -> Rc<Pool> {
    let config = block::Config { budget: 1 << 20 };
    let memory = block::Heap::new(config.reservation());
    Rc::new(Pool::new(config, memory))
}

/// `body` from `from` to `to` in `TERM`, with no proof.
pub(crate) fn message(from: u8, to: u8, body: Body) -> Message {
    Message {
        from: key(from),
        to: key(to),
        term: TERM,
        body,
        proof: None,
        chain: Vec::new(),
    }
}

pub(crate) fn reply_body(grant: Grant, answer: Answer) -> Body {
    match grant {
        Grant::PreVote => Body::PreVoteReply { answer },
        Grant::Vote => Body::VoteReply { answer },
    }
}

/// `voter`'s grant to `candidate`, signed as `sign` signs a reply.
pub(crate) fn granted(voter: u8, grant: Grant, candidate: u8) -> Message {
    let body = reply_body(grant, Answer::Granted(None));
    let mut ready = Ready {
        messages: vec![message(voter, candidate, body)],
        ..Ready::default()
    };
    signer(voter).sign(&mut ready);
    ready.messages.remove(0)
}

/// `voter`'s signature of its `grant` to `candidate` in `TERM`.
pub(crate) fn signature(voter: u8, grant: Grant, candidate: u8) -> Signature {
    grant_in(TERM, voter, grant, candidate)
}

/// `voter`'s signature of its `grant` to `candidate` in `term`.
pub(crate) fn grant_in(
    term: Term,
    voter: u8,
    grant: Grant,
    candidate: u8,
) -> Signature {
    grant_signed(term, voter, voter, grant, candidate)
}

/// The signature of the `grant` of `voter` to `candidate` in `term`, made with the
/// private key of `signer`: a forgery unless `signer` is `voter`.
pub(crate) fn grant_signed(
    term: Term,
    voter: u8,
    signer: u8,
    grant: Grant,
    candidate: u8,
) -> Signature {
    let body = reply_body(grant, Answer::Granted(None));
    let mut ready = Ready {
        messages: vec![Message {
            term,
            ..message(voter, candidate, body)
        }],
        ..Ready::default()
    };
    Signer::new(key(voter), &private(signer)).sign(&mut ready);
    match ready.messages.remove(0).body {
        Body::PreVoteReply {
            answer: Answer::Granted(signature),
        }
        | Body::VoteReply {
            answer: Answer::Granted(signature),
        } => signature.expect("`sign` signs the grant"),
        body => unreachable!("a grant answers with {body:?}"),
    }
}

/// `body` from `leader` to `to`, with the leader's votes from 1, 2 and 3, signed.
///
/// # Panics
///
/// When `leader` is not 1, 2 or 3: a proof holds its candidate as a voter.
pub(crate) fn proven(leader: u8, to: u8, body: Body) -> Message {
    proven_in(TERM, leader, to, body)
}

/// As [`proven`], in `term`.
pub(crate) fn proven_in(term: Term, leader: u8, to: u8, body: Body) -> Message {
    assert!((1..=3).contains(&leader), "leader {leader} is not a voter");
    proven_at(
        leader,
        to,
        term,
        &[1, 2, 3].map(|voter| (voter, voter)),
        body,
    )
}

/// `body` from `leader` to `to` in `term`, signed, with the leader's vote from each
/// `(voter, signer)`, which the private key of `signer` signs. The leader's own vote
/// carries no signature.
pub(crate) fn proven_at(
    leader: u8,
    to: u8,
    term: Term,
    votes: &[(u8, u8)],
    body: Body,
) -> Message {
    let vote = |&(voter, signer): &(u8, u8)| {
        let signed = (voter != leader)
            .then(|| grant_signed(term, voter, signer, Grant::Vote, leader));
        (key(voter), signed)
    };
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(leader),
        voters: votes.iter().map(vote).collect(),
    };
    let mut ready = Ready {
        messages: vec![Message {
            term,
            proof: Some(proof),
            ..message(leader, to, body)
        }],
        ..Ready::default()
    };
    signer(leader).sign(&mut ready);
    ready.messages.remove(0)
}

pub(crate) fn index(bits: u128) -> channel::Key {
    channel::Key::from_u128(bits)
}

pub(crate) fn name(text: &str) -> Name {
    text.parse().unwrap()
}

pub(crate) fn options(prefix: &str, reusable: bool) -> Options {
    Options {
        prefix: name(prefix),
        reusable,
        expiry: EXPIRY,
        ephemeral: Some(EPHEMERAL),
    }
}

pub(crate) fn record(id: u8, options: Options) -> Change {
    Change::Ticket {
        public_key: public(id),
        options,
    }
}

/// The join of node `id` with the name `name_text`, which ticket `ticket_id` admits.
pub(crate) fn join(ticket_id: u8, id: u8, name_text: &str) -> Join {
    let card = signed(id, name_text);
    Join {
        ticket: public(ticket_id),
        at: BEFORE_EXPIRY,
        card: card::Unchecked {
            key: key(id),
            card: card.card().clone(),
            signature: *card.signature(),
        },
        admission: ticket(ticket_id).admission(&card),
        status: status([(name("disk"), index(9))]),
    }
}

pub(crate) fn home(i: u128, h: u128) -> Change {
    Change::Home {
        index: index(i),
        home: node::Key::from_u128(h),
    }
}

pub(crate) fn digest(byte: u8) -> Digest {
    Digest([byte; 32])
}

/// A spec change on version `version` at root `[base; 32]`, to root `[root; 32]`, with
/// each chunk `[chunk; 32]` of `chunks`.
pub(crate) fn spec(version: u64, base: u8, root: u8, chunks: &[u8]) -> Change {
    Change::Spec {
        base: Pointer {
            version,
            root: digest(base),
        },
        root: digest(root),
        chunks: chunks.iter().copied().map(digest).collect(),
    }
}

pub(crate) fn with_status(mut join: Join, status: &[(&str, u128)]) -> Join {
    let map = status.iter().map(|&(text, key)| (name(text), index(key)));
    join.status = Status::new(map.collect()).unwrap();
    join
}

// The vote of each of `voted` for `leader` in `term`, each signed but the leader's
// own.
fn votes(term: Term, leader: u8, voted: &[u8]) -> Proof {
    let vote = |&voter: &u8| {
        let signed =
            (voter != leader).then(|| grant_in(term, voter, Grant::Vote, leader));
        (key(voter), signed)
    };
    Proof {
        grant: Grant::Vote,
        candidate: key(leader),
        voters: voted.iter().map(vote).collect(),
    }
}

/// The configuration entry `voters` that `leader` wrote at `at`, with its votes from
/// 1, 2 and 3 in the term of `at`, signed as `sign` signs a change.
///
/// # Panics
///
/// When `leader` is not 1, 2 or 3, as [`proven`].
pub(crate) fn change(leader: u8, at: Position, voters: Voters) -> Entry {
    assert!((1..=3).contains(&leader), "leader {leader} is not a voter");
    change_voted(leader, at, voters, &[1, 2, 3])
}

/// The configuration entry `voters` that `leader` wrote at `at`, with the vote of
/// each of `voted` in the term of `at`, signed as `sign` signs a change.
pub(crate) fn change_voted(
    leader: u8,
    at: Position,
    voters: Voters,
    voted: &[u8],
) -> Entry {
    let entry = Entry {
        at,
        data: Data::Voters(raft::Change {
            voters,
            votes: votes(at.term, leader, voted),
            signature: None,
        }),
    };
    let mut ready = Ready {
        entries: vec![entry],
        ..Ready::default()
    };
    signer(leader).sign(&mut ready);
    ready.entries.remove(0)
}
