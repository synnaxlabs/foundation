//! Test helpers that the modules of `mesh` reuse: keys, signed messages, and a pool.
//! Node `id` has the private key `[id; 32]`.

use std::collections::BTreeMap;
use std::rc::Rc;

use block::Pool;
use raft::{Answer, Body, Grant, Message, Proof, Ready, Signature, Term};
use types::node::{self, PrivateKey, PublicKey, SealKey};

use crate::card::{self, Card};
use crate::ed25519;
use crate::grant::Signer;
use crate::member::Member;

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

/// The record of node `id`, with a card that the node signed.
pub(crate) fn member(id: u8) -> Member {
    let card = Card {
        name: format!("plant.node{id}").parse().unwrap(),
        public_key: public(id),
        seal_key: SealKey::new([9; 32]).unwrap(),
        addresses: card::addresses::Addresses::new(Vec::new()).unwrap(),
        version: 1,
    };
    Member {
        card: card::Signed::sign(key(id), card, &private(id)),
        admission: [0; 64],
        ephemeral: None,
        status: BTreeMap::new(),
    }
}

pub(crate) fn create_members(ids: &[u8]) -> Vec<Member> {
    ids.iter().map(|&id| member(id)).collect()
}

/// A pool of 4 MiB.
pub(crate) fn create_pool() -> Rc<Pool> {
    let config = block::Config { budget: 4 << 20 };
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
