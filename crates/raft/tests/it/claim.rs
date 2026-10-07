//! What each signature in a message attests, and how `Ready::sign` fills them.

use proptest::prelude::*;
use raft::{
    Answer, Body, Change, Claim, Data, Entry, Grant, Hard, Message, Position, Proof,
    Ready, Signature, Term, Voters,
};
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
    let message = message(7, body, Some(proof));
    let claims: Vec<_> = message.claims().collect();
    let claim = |voter, grant, candidate| Claim::Grant {
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
fn an_append_claims_each_vote_then_the_change_of_each_change_it_carries() {
    let kept = change(&|voter| Some(signature(voter)));
    let bytes = Entry {
        at: Position {
            term: Term(5),
            index: 4,
        },
        data: Data::Bytes(vec![1]),
    };
    let append = Body::Append {
        prev: Position::default(),
        entries: vec![bytes, kept.clone(), kept],
        commit: 0,
    };
    let message = message(6, append, None);
    let claims: Vec<_> = message.claims().collect();
    let voters = Voters {
        incoming: [key(1), key(2)].into(),
        outgoing: [key(1), key(2), key(3)].into(),
    };
    let vote = |voter| Claim::Grant {
        voter: key(voter),
        grant: Grant::Vote,
        term: Term(5),
        candidate: key(2),
    };
    let change = Claim::Change {
        leader: key(2),
        at: Position {
            term: Term(5),
            index: 3,
        },
        voters: &voters,
    };
    let one = [
        (vote(1), Some(signature(1))),
        (vote(2), Some(signature(2))),
        (change, Some(signature(0))),
    ];
    assert_eq!(claims, [one, one].concat());
}

#[test]
fn a_pre_vote_grant_claims_a_pre_vote_to_the_receiver() {
    let body = Body::PreVoteReply {
        answer: Answer::Granted(None),
    };
    let message = message(4, body, None);
    let claims: Vec<_> = message.claims().collect();
    let claim = Claim::Grant {
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
        signed.push(Network::signature(claim));
        Network::signature(claim)
    });
    let own = Claim::Grant {
        voter: key(1),
        grant: Grant::Vote,
        term: Term(5),
        candidate: key(1),
    };
    let granted = Claim::Grant {
        voter: key(1),
        grant: Grant::PreVote,
        term: Term(6),
        candidate: key(2),
    };
    let want = [Network::signature(&own), Network::signature(&granted)];
    assert_eq!(signed, want);
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

// The change of leader 2 at index 3 of term 5, with each signature `with` gives.
fn change(with: &dyn Fn(u8) -> Option<Signature>) -> Entry {
    let voters = Voters {
        incoming: [key(1), key(2)].into(),
        outgoing: [key(1), key(2), key(3)].into(),
    };
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(2),
        voters: [(key(1), with(1)), (key(2), with(2))].into(),
    };
    Entry {
        at: Position {
            term: Term(5),
            index: 3,
        },
        data: Data::Voters(Change {
            voters,
            votes: proof,
            signature: with(0),
        }),
    }
}

#[test]
fn sign_fills_each_change_in_entries_committed_and_appends_and_keeps_the_rest() {
    let unsigned = change(&|_| None);
    let kept = change(&|voter| Some(signature(voter)));
    let append = |entries| Body::Append {
        prev: Position::default(),
        entries,
        commit: 0,
    };
    let mut ready = Ready {
        entries: vec![unsigned.clone(), kept.clone()],
        committed: vec![unsigned.clone()],
        messages: vec![message(5, append(vec![kept.clone(), unsigned]), None)],
        ..Ready::default()
    };
    ready.sign(Network::signature);
    let own = |voter: u8| {
        let claim = match voter {
            0 => Claim::Change {
                leader: key(2),
                at: Position {
                    term: Term(5),
                    index: 3,
                },
                voters: &Voters {
                    incoming: [key(1), key(2)].into(),
                    outgoing: [key(1), key(2), key(3)].into(),
                },
            },
            voter => Claim::Grant {
                voter: key(u128::from(voter)),
                grant: Grant::Vote,
                term: Term(5),
                candidate: key(2),
            },
        };
        Some(Network::signature(&claim))
    };
    let signed = change(&own);
    assert_eq!(ready.entries, [signed.clone(), kept.clone()]);
    assert_eq!(ready.committed, std::slice::from_ref(&signed));
    assert_eq!(ready.messages[0].body, append(vec![kept, signed]));
}

proptest! {
    #[test]
    fn sign_gives_each_claim_a_signature_and_keeps_each_it_had(
        term in any::<u64>(),
        body in body(),
        proof in prop::option::of(proof(5)),
    ) {
        // A claim borrows its message, so each is kept as its stand-in signature.
        let signed = |message: &Message| -> Vec<(Signature, Option<Signature>)> {
            message
                .claims()
                .map(|(claim, had)| (Network::signature(&claim), had))
                .collect()
        };
        let message = message(term, body, proof);
        let before = signed(&message);
        let mut ready = Ready {
            messages: vec![message],
            ..Ready::default()
        };
        ready.sign(Network::signature);
        let after = signed(&ready.messages[0]);
        let want: Vec<_> = before
            .into_iter()
            .map(|(claim, had)| (claim, had.or(Some(claim))))
            .collect();
        prop_assert_eq!(after, want);
    }
}
