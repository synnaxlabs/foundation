//! Test helpers that the modules of `mesh` reuse: keys, signed messages, and a pool.
//! Node `id` has the private key `[id; 32]`.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use block::Pool;
use raft::{
    Answer, Body, Data, Entry, Grant, Message, Position, Proof, Ready, Signature, Term,
    Voters,
};
use spec::Pointer;
use transport::Address;
use types::channel;
use types::digest::Digest;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;
use types::node::{self, SealKey};
use types::time::{Span, Stamp};

use crate::bytes::{put_channel, put_count, put_name};
use crate::card::addresses::{self, Addresses};
use crate::card::{self, Card};
use crate::change::{CHUNKS_MAX, Change, HOLDERS_MAX, HOMES_MAX, Join};
use crate::claim::Signer;
use crate::member::Member;
use crate::region::Request;
use crate::status::{self, Status};
use crate::ticket::{Options, Ticket, Voter};

#[path = "../tests/common/steps.rs"]
mod steps;

pub(crate) use steps::simulation;

/// The term of each message.
pub(crate) const TERM: Term = Term(5);

pub(crate) const EXPIRY: Stamp = Stamp::from_nanos(1_000);
pub(crate) const BEFORE_EXPIRY: Stamp = Stamp::from_nanos(999);
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

/// The public key of each voter that a change to `voters` adds, as its leader binds
/// them: none in a leave. The key of a node is that of the node `id` of its low byte.
pub(crate) fn public_keys(voters: &Voters) -> BTreeMap<node::Key, PublicKey> {
    let id = |node: node::Key| node.as_u128().to_le_bytes()[0];
    voters
        .added()
        .map(|node| (node, public(id(node))))
        .collect()
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

/// The member `id`, with a card that the node signed.
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

/// The byte form of a `Join` of `request` with the status names `names` in place of
/// its own, also more than 64.
pub(crate) fn join_bytes(request: Request, names: &[&str]) -> Vec<u8> {
    let request = Box::new(Request {
        status: status::Names::default(),
        ..request
    });
    let mut bytes = crate::message::Message::Join { request }.encode();
    bytes.truncate(bytes.len().saturating_sub(8));
    let names: BTreeSet<_> = names.iter().map(|text| name(text)).collect();
    put_count(names.len(), &mut bytes);
    for name in &names {
        put_name(name, &mut bytes);
    }
    bytes
}

/// The status names `names`, at most 64.
pub(crate) fn names(names: impl IntoIterator<Item = Name>) -> status::Names {
    status::Names::new(names.into_iter().collect()).unwrap()
}

pub(crate) fn create_members(ids: &[u8]) -> Vec<Member> {
    ids.iter().map(|&id| member(id)).collect()
}

/// A pool of 1 MiB. It reserves 51 MiB.
pub(crate) fn create_pool() -> Rc<Pool> {
    let config = block::Config::new(1 << 20).expect("the budget fits");
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

/// The change that records the ticket of node `id` with `options`.
pub(crate) fn ticket_change(id: u8, options: Options) -> Change {
    Change::Ticket {
        public_key: public(id),
        options,
    }
}

/// The join of node `id` with the name `name_text`, which ticket `ticket_id` admits.
pub(crate) fn join(ticket_id: u8, id: u8, name_text: &str) -> Join {
    let status = status([(name("disk"), index(9))]);
    join_with(&signed(id, name_text), ticket_id, BEFORE_EXPIRY, status)
}

/// The join at `at` of the node of `card` with `status`, which ticket `ticket_id`
/// admits.
pub(crate) fn join_with(
    card: &card::Signed,
    ticket_id: u8,
    at: Stamp,
    status: Status,
) -> Join {
    Join {
        ticket: public(ticket_id),
        at,
        card: unchecked(card),
        admission: ticket(ticket_id).admission(card),
        status,
    }
}

/// `card` in the form a request or a join carries.
pub(crate) fn unchecked(card: &card::Signed) -> card::Unchecked {
    card::Unchecked {
        key: card.key(),
        card: card.card().clone(),
        signature: *card.signature(),
    }
}

/// The largest change of each kind: a home, a join, a ticket, a spec at each bound,
/// and a card.
pub(crate) fn create_largest() -> [Change; 5] {
    // A name of the most bytes that ends in `at`.
    let long = |at: usize| {
        let end = format!("b{at}");
        name(&format!("{end:a>width$}", width = Name::MAX_BYTES))
    };
    let relay = Address::Relay {
        node: public(2),
        at: "[::1]:4000".parse().unwrap(),
    };
    let card = Card {
        name: long(0),
        public_key: public(3),
        seal_key: SealKey::new([9; 32]).unwrap(),
        addresses: Addresses::new(vec![relay; addresses::MAX.into()]).unwrap(),
        version: u64::MAX,
    };
    let card = card::Signed::sign(key(3), card, &private(3));
    let statuses = (0..usize::from(status::MAX)).map(|at| (long(at), index(9)));
    let status = Status::new(statuses.collect()).unwrap();
    let join = join_with(&card, 7, BEFORE_EXPIRY, status);
    let card = Change::Card(Box::new(unchecked(&card)));
    let mut options = options("a", true);
    options.prefix = long(0);
    let chunks = (0..CHUNKS_MAX).map(|at| {
        let mut chunk = [0; 32];
        chunk[..2].copy_from_slice(&u16::try_from(at).unwrap().to_be_bytes());
        Digest(chunk)
    });
    let holders = 0..u128::try_from(HOLDERS_MAX).unwrap();
    let homes = 0..u128::try_from(HOMES_MAX).unwrap();
    let spec = Change::Spec {
        base: Pointer {
            version: 7,
            root: digest(1),
        },
        root: digest(2),
        chunks: chunks.collect(),
        holders: holders.map(node::Key::from_u128).collect(),
        homes: homes.map(|at| (index(at), key(1))).collect(),
    };
    [
        home(u128::MAX, u128::MAX),
        Change::Join(Box::new(join)),
        ticket_change(7, options),
        spec,
        card,
    ]
}

/// The byte form of `change`.
pub(crate) fn encoded(change: &Change) -> Vec<u8> {
    let mut out = Vec::new();
    change.encode(&mut out);
    out
}

/// The position `index` of `term`.
pub(crate) fn at(term: u64, index: u64) -> Position {
    Position {
        term: Term(term),
        index,
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
/// each chunk `[chunk; 32]` of `chunks`, held by node 1.
pub(crate) fn spec(version: u64, base: u8, root: u8, chunks: &[u8]) -> Change {
    Change::Spec {
        base: Pointer {
            version,
            root: digest(base),
        },
        root: digest(root),
        chunks: chunks.iter().copied().map(digest).collect(),
        holders: [key(1)].into(),
        homes: BTreeMap::new(),
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
pub(crate) fn voters_entry(leader: u8, at: Position, voters: Voters) -> Entry {
    assert!((1..=3).contains(&leader), "leader {leader} is not a voter");
    voters_entry_voted(leader, at, voters, &[1, 2, 3])
}

/// The configuration entry `voters` that `leader` wrote at `at`, with the vote of
/// each of `voted` in the term of `at`, signed as `sign` signs a change.
pub(crate) fn voters_entry_voted(
    leader: u8,
    at: Position,
    voters: Voters,
    voted: &[u8],
) -> Entry {
    let entry = Entry {
        at,
        data: Data::Voters(raft::Change {
            public_keys: public_keys(&voters),
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
