//! What each signature in a message attests, and how `Ready::sign` fills them.

use proptest::prelude::*;
use raft::{Answer, Body, Claim, Grant, Hard, Message, Proof, Ready, Signature, Term};
use types::node;

use crate::check::{body, proof};
use crate::network::Network;

fn key(node: u128) -> node::Key {
    node::Key::from_u128(node)
}

fn signature(byte: u8) -> Signature {
    Signature([byte; 64])
}

fn message(term: u64, body: Body, proof: Option<Proof>) -> Message {
    Message {
        from: key(1),
        to: key(2),
        term: Term(term),
        body,
        proof,
    }
}

#[test]
fn a_message_claims_its_proof_in_its_term_then_its_grant() {
    let proof = Proof {
        grant: Grant::PreVote,
        candidate: key(3),
        voters: [(key(3), Some(signature(3))), (key(1), Some(signature(1)))].into(),
    };
    let body = Body::VoteReply {
        answer: Answer::Granted(Some(signature(9))),
    };
    let claims: Vec<_> = message(7, body, Some(proof)).claims().collect();
    let claim = |voter, grant, candidate| Claim {
        voter: key(voter),
        grant,
        term: Term(7),
        candidate: key(candidate),
    };
    let want = vec![
        (claim(1, Grant::PreVote, 3), Some(signature(1))),
        (claim(3, Grant::PreVote, 3), Some(signature(3))),
        (claim(1, Grant::Vote, 2), Some(signature(9))),
    ];
    assert_eq!(claims, want);
}

#[test]
fn a_pre_vote_grant_claims_a_pre_vote_to_the_receiver() {
    let body = Body::PreVoteReply {
        answer: Answer::Granted(None),
    };
    let claims: Vec<_> = message(4, body, None).claims().collect();
    let claim = Claim {
        voter: key(1),
        grant: Grant::PreVote,
        term: Term(4),
        candidate: key(2),
    };
    assert_eq!(claims, vec![(claim, None)]);
}

#[test]
fn a_message_that_grants_nothing_claims_nothing() {
    let refusal = Body::VoteReply {
        answer: Answer::Refused,
    };
    assert_eq!(message(4, refusal, None).claims().count(), 0);
    let vote = Body::Vote {
        last: raft::Position::default(),
    };
    assert_eq!(message(4, vote, None).claims().count(), 0);
}

#[test]
fn sign_fills_the_hard_proof_in_its_term_and_each_grant_in_its_message_term() {
    let hard = Hard {
        term: Term(5),
        proof: Some(Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [(key(1), None), (key(2), Some(signature(2)))].into(),
        }),
        ..Hard::default()
    };
    let refusal = Body::VoteReply {
        answer: Answer::Refused,
    };
    let grant = Body::PreVoteReply {
        answer: Answer::Granted(None),
    };
    let mut ready = Ready {
        hard: Some(hard),
        messages: vec![message(6, refusal.clone(), None), message(6, grant, None)],
        ..Ready::default()
    };
    let mut signed = Vec::new();
    ready.sign(|claim| {
        signed.push(*claim);
        Network::signature(claim)
    });
    let own = Claim {
        voter: key(1),
        grant: Grant::Vote,
        term: Term(5),
        candidate: key(1),
    };
    let granted = Claim {
        voter: key(1),
        grant: Grant::PreVote,
        term: Term(6),
        candidate: key(2),
    };
    assert_eq!(signed, vec![own, granted]);
    let voters = &ready.hard.unwrap().proof.unwrap().voters;
    let want = [
        (key(1), Some(Network::signature(&own))),
        (key(2), Some(signature(2))),
    ];
    assert_eq!(*voters, want.into());
    assert_eq!(ready.messages[0].body, refusal);
    let answer = Answer::Granted(Some(Network::signature(&granted)));
    assert_eq!(ready.messages[1].body, Body::PreVoteReply { answer });
}

proptest! {
    #[test]
    fn sign_gives_each_claim_a_signature_and_keeps_each_it_had(
        term in any::<u64>(),
        body in body(),
        proof in prop::option::of(proof(5)),
    ) {
        let message = message(term, body, proof);
        let before: Vec<_> = message.claims().collect();
        let mut ready = Ready {
            messages: vec![message],
            ..Ready::default()
        };
        ready.sign(Network::signature);
        let after: Vec<_> = ready.messages[0].claims().collect();
        let want: Vec<_> = before
            .into_iter()
            .map(|(claim, had)| (claim, had.or(Some(Network::signature(&claim)))))
            .collect();
        prop_assert_eq!(after, want);
    }
}
