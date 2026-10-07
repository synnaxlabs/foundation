//! Keys, signed messages, and a pool for tests. Node `id` has the private key
//! `[id; 32]`, and each message is in term 5.

use std::collections::BTreeMap;
use std::rc::Rc;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::Pool;
use raft::{Answer, Body, Grant, Message, Proof, Ready, Signature, Term};
use types::node::{self, PrivateKey, PublicKey};

use crate::grant::Signer;

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
    let pair = Ed25519KeyPair::from_seed_unchecked(&private(id).0).unwrap();
    PublicKey::new(pair.public_key().as_ref().try_into().unwrap()).unwrap()
}

pub(crate) fn members(ids: &[u8]) -> BTreeMap<node::Key, PublicKey> {
    ids.iter().map(|&id| (key(id), public(id))).collect()
}

/// A pool of 4 MiB.
pub(crate) fn pool() -> Rc<Pool> {
    let config = block::Config { budget: 4 << 20 };
    let memory = block::Heap::new(config.reservation());
    Rc::new(Pool::new(config, memory))
}

pub(crate) fn message(from: u8, to: u8, body: Body, proof: Option<Proof>) -> Message {
    Message {
        from: key(from),
        to: key(to),
        term: Term(5),
        body,
        proof,
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
        messages: vec![message(voter, candidate, body, None)],
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
pub(crate) fn proven(leader: u8, to: u8, body: Body) -> Message {
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
        messages: vec![message(leader, to, body, Some(proof))],
        ..Ready::default()
    };
    signer(leader).sign(&mut ready);
    ready.messages.remove(0)
}
