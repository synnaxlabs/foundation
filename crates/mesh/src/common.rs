//! Test helpers that the modules of `mesh` reuse: keys, signed messages, and a pool.
//! Node `id` has the private key `[id; 32]`.

use std::rc::Rc;

use block::Pool;
use raft::{
    Answer, Body, Change, Data, Entry, Grant, Message, Position, Proof, Ready,
    Signature, Term, Voters,
};
use transport::Address;
use types::channel;
use types::name::Name;
use types::node::{self, PrivateKey, PublicKey, SealKey};

use crate::bytes::{put_channel, put_count, put_name};
use crate::card::{self, Card};
use crate::claim::Signer;
use crate::ed25519;
use crate::member::Member;
use crate::status::Status;
use crate::ticket::{Ticket, Voter};

/// The term of each message.
pub(crate) const TERM: Term = Term(5);

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
    ed25519::public(&ed25519::pair(&private(id)))
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

pub(crate) fn signature(voter: u8, grant: Grant, candidate: u8) -> Signature {
    let (_, signature) = granted(voter, grant, candidate).claims().next().unwrap();
    signature.unwrap()
}

/// `body` from `leader` to `to`, with the leader's votes from 1, 2 and 3, signed.
///
/// # Panics
///
/// When `leader` is not 1, 2 or 3: a proof holds its candidate as a voter.
pub(crate) fn proven(leader: u8, to: u8, body: Body) -> Message {
    assert!((1..=3).contains(&leader), "leader {leader} is not a voter");
    let vote = |voter| {
        let signed = (voter != leader).then(|| signature(voter, Grant::Vote, leader));
        (key(voter), signed)
    };
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(leader),
        voters: [1, 2, 3].map(vote).into(),
    };
    let mut ready = Ready {
        messages: vec![Message {
            proof: Some(proof),
            ..message(leader, to, body)
        }],
        ..Ready::default()
    };
    signer(leader).sign(&mut ready);
    ready.messages.remove(0)
}

/// The configuration entry `voters` that `leader` wrote at `at`, with its votes from
/// 1, 2 and 3, signed as `sign` signs a change.
///
/// # Panics
///
/// When `leader` is not 1, 2 or 3, as [`proven`].
pub(crate) fn change(leader: u8, at: Position, voters: Voters) -> Entry {
    let to = if leader == 1 { 2 } else { 1 };
    let proof = proven(leader, to, Body::HeartbeatReply).proof;
    let entry = Entry {
        at,
        data: Data::Voters(Change {
            voters,
            votes: proof.expect("a proven message holds a proof"),
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
