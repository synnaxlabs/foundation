//! What each signature in a message attests, in the order the receiver reads them,
//! and how `Ready::sign` fills them.

use proptest::prelude::*;
use raft::{
    Answer, Body, Change, Claim, Config, Data, Entry, Error, Grant, Hard, Link,
    Message, Position, Proof, Raft, Ready, Signature, Start, Term, Voters,
};
use types::node;

use crate::check::{body, position, proof};
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
        chain: Vec::new(),
    }
}

fn voters(ids: &[u128]) -> Voters {
    Voters {
        incoming: ids.iter().copied().map(key).collect(),
        ..Voters::default()
    }
}

// Node 2, with voters 1, 2, and 3, in `term`.
fn receiver(term: u64) -> Raft {
    let config = Config {
        key: key(2),
        election_ticks: 10,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: Hard {
            term: Term(term),
            ..Hard::default()
        },
        voters: voters(&[1, 2, 3]),
        ..Start::default()
    };
    Raft::new(config, start).unwrap()
}

fn claims(raft: &Raft, message: &Message) -> Vec<(Claim<'static>, Option<Signature>)> {
    raft.claims(message)
        .map(|(claim, signature)| (own(claim), signature))
        .collect()
}

// A claim that borrows nothing: the voters of a change are leaked.
fn own(claim: Claim<'_>) -> Claim<'static> {
    match claim {
        Claim::Grant {
            voter,
            grant,
            term,
            candidate,
        } => Claim::Grant {
            voter,
            grant,
            term,
            candidate,
        },
        Claim::Change { leader, at, voters } => Claim::Change {
            leader,
            at,
            voters: Box::leak(Box::new(voters.clone())),
        },
    }
}

fn grant(voter: u128, grant: Grant, term: u64, candidate: u128) -> Claim<'static> {
    Claim::Grant {
        voter: key(voter),
        grant,
        term: Term(term),
        candidate: key(candidate),
    }
}

// The change of `leader` at index `index` of term `term`, with the votes of
// `elected` for it, each signed with its own byte, and the change signed with the
// leader's.
fn link(term: u64, index: u64, leader: u128, elected: &[u128], ids: &[u128]) -> Link {
    let voter = |&id: &u128| (key(id), Some(signature(u8::try_from(id).unwrap())));
    Link {
        at: Position {
            term: Term(term),
            index,
        },
        change: Change {
            voters: voters(ids),
            votes: Proof {
                grant: Grant::Vote,
                candidate: key(leader),
                voters: elected.iter().map(voter).collect(),
            },
            signature: Some(signature(u8::try_from(leader).unwrap())),
        },
    }
}

// The claims of `link`: each vote in its term, then the change.
fn link_claims(link: &Link) -> Vec<(Claim<'static>, Option<Signature>)> {
    let at = link.at;
    let leader = link.change.votes.candidate.as_u128();
    let mut claims: Vec<_> = link
        .change
        .votes
        .voters
        .iter()
        .map(|(voter, signature)| {
            (
                grant(voter.as_u128(), Grant::Vote, at.term.0, leader),
                *signature,
            )
        })
        .collect();
    let change = Claim::Change {
        leader: key(leader),
        at,
        voters: Box::leak(Box::new(link.change.voters.clone())),
    };
    claims.push((change, link.change.signature));
    claims
}

// A proof of the vote of 1 and 4 for 1 in `term`: no quorum of the receiver's
// voters.
fn outside(term: u64) -> Message {
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(1),
        voters: [(key(1), Some(signature(1))), (key(4), Some(signature(4)))].into(),
    };
    message(term, Body::Heartbeat { commit: 0 }, Some(proof))
}

// The chain of a leader that 1 and 4 elect: 3 moved the voters to {1, 2, 3, 4},
// then 1 moved them to {1, 4, 5}, then to {1, 4, 5, 6, 7}. The second link is the
// first whose configuration the votes of 1 and 4 are a quorum of.
fn chain() -> Vec<Link> {
    vec![
        link(1, 1, 3, &[2, 3], &[1, 2, 3, 4]),
        link(2, 2, 1, &[1, 2, 3], &[1, 4, 5]),
        link(3, 3, 1, &[1, 4], &[1, 4, 5, 6, 7]),
    ]
}

#[test]
fn a_message_claims_its_proof_then_each_link_read_then_its_changes() {
    // The receiver's log is empty, so the change goes at index 1.
    let mut kept = change(&|voter| Some(signature(voter)));
    kept.at.index = 1;
    let mut message = outside(7);
    message.body = Body::Append {
        prev: Position::default(),
        entries: vec![kept],
        commit: 0,
    };
    message.chain = chain();
    let raft = receiver(0);
    let mut want = vec![
        (grant(1, Grant::Vote, 7, 1), Some(signature(1))),
        (grant(4, Grant::Vote, 7, 1), Some(signature(4))),
    ];
    want.extend(link_claims(&message.chain[0]));
    want.extend(link_claims(&message.chain[1]));
    let voters = voters(&[1, 2]);
    let voters = Voters {
        outgoing: [key(1), key(2), key(3)].into(),
        ..voters
    };
    want.extend([
        (grant(1, Grant::Vote, 5, 2), Some(signature(1))),
        (grant(2, Grant::Vote, 5, 2), Some(signature(2))),
        (
            Claim::Change {
                leader: key(2),
                at: Position {
                    term: Term(5),
                    index: 1,
                },
                voters: Box::leak(Box::new(voters)),
            },
            Some(signature(0)),
        ),
    ]);
    assert_eq!(claims(&raft, &message), want);
    let mut raft = raft;
    raft.step(message).unwrap();
    assert_eq!((raft.term(), raft.leader()), (Term(7), Some(key(1))));
}

#[test]
fn a_reply_claims_its_proof_then_each_link_read_then_its_grant() {
    let mut message = outside(7);
    message.body = Body::VoteReply {
        answer: Answer::Granted(Some(signature(9))),
    };
    message.chain = chain();
    let mut want = vec![
        (grant(1, Grant::Vote, 7, 1), Some(signature(1))),
        (grant(4, Grant::Vote, 7, 1), Some(signature(4))),
    ];
    want.extend(link_claims(&message.chain[0]));
    want.extend(link_claims(&message.chain[1]));
    want.push((grant(1, Grant::Vote, 7, 2), Some(signature(9))));
    assert_eq!(claims(&receiver(0), &message), want);
}

#[test]
fn a_message_claims_no_link_when_its_proof_is_a_quorum_of_the_voters() {
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(1),
        voters: [(key(1), Some(signature(1))), (key(3), Some(signature(3)))].into(),
    };
    let mut message = message(7, Body::Heartbeat { commit: 0 }, Some(proof));
    message.chain = chain();
    let want = vec![
        (grant(1, Grant::Vote, 7, 1), Some(signature(1))),
        (grant(3, Grant::Vote, 7, 1), Some(signature(3))),
    ];
    assert_eq!(claims(&receiver(0), &message), want);
}

#[test]
fn a_message_claims_each_link_when_its_chain_does_not_prove_it() {
    let mut message = outside(7);
    message.chain = chain()[..1].to_vec();
    let mut want = vec![
        (grant(1, Grant::Vote, 7, 1), Some(signature(1))),
        (grant(4, Grant::Vote, 7, 1), Some(signature(4))),
    ];
    want.extend(link_claims(&message.chain[0]));
    let mut raft = receiver(0);
    assert_eq!(claims(&raft, &message), want);
    let expected = Error::Unproven {
        term: Term(7),
        from: key(1),
    };
    assert_eq!(raft.step(message), Err(expected));
}

// A stale message is answered, not read, so a forged grant in it is no claim.
#[test]
fn a_stale_message_claims_nothing_and_is_answered() {
    let mut raft = receiver(7);
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(3),
        voters: [(key(2), Some(signature(2))), (key(3), Some(signature(3)))].into(),
    };
    let mut led = message(8, Body::Heartbeat { commit: 0 }, Some(proof.clone()));
    led.from = key(3);
    raft.step(led).unwrap();
    drop(raft.ready());
    let mut stale = outside(7);
    stale.chain = chain();
    assert_eq!(claims(&raft, &stale), Vec::new());
    assert_eq!(raft.step(stale), Ok(()));
    let ready = raft.ready();
    let [reply] = &ready.messages[..] else {
        panic!("one reply: {:?}", ready.messages);
    };
    let sent = (reply.to, reply.term, &reply.body, reply.proof.as_ref());
    assert_eq!(sent, (key(1), Term(8), &Body::HeartbeatReply, Some(&proof)));
}

// A reply from a node that is not a peer is dropped, not read.
#[test]
fn a_reply_from_a_node_that_is_not_a_peer_claims_nothing() {
    let mut message = outside(7);
    message.from = key(9);
    message.body = Body::VoteReply {
        answer: Answer::Granted(Some(signature(9))),
    };
    message.chain = chain();
    let mut raft = receiver(0);
    assert_eq!(claims(&raft, &message), Vec::new());
    assert_eq!(raft.step(message), Ok(()));
    assert_eq!(raft.ready(), Ready::default());
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
    let claims = claims(&receiver(0), &message);
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
    let claims = claims(&receiver(0), &message);
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
fn an_append_claims_its_proof_then_its_changes() {
    let proof = Proof {
        grant: Grant::Vote,
        candidate: key(1),
        voters: [(key(1), Some(signature(1))), (key(3), Some(signature(3)))].into(),
    };
    let kept = change(&|voter| Some(signature(voter)));
    let append = Body::Append {
        prev: Position::default(),
        entries: vec![kept],
        commit: 0,
    };
    let message = message(6, append, Some(proof));
    let claims = claims(&receiver(0), &message);
    let voters = Voters {
        incoming: [key(1), key(2)].into(),
        outgoing: [key(1), key(2), key(3)].into(),
    };
    let grant = |voter, term, candidate| Claim::Grant {
        voter: key(voter),
        grant: Grant::Vote,
        term: Term(term),
        candidate: key(candidate),
    };
    let change = Claim::Change {
        leader: key(2),
        at: Position {
            term: Term(5),
            index: 3,
        },
        voters: &voters,
    };
    let want = [
        (grant(1, 6, 1), Some(signature(1))),
        (grant(3, 6, 1), Some(signature(3))),
        (grant(1, 5, 2), Some(signature(1))),
        (grant(2, 5, 2), Some(signature(2))),
        (change, Some(signature(0))),
    ];
    assert_eq!(claims, want);
}

#[test]
fn a_pre_vote_grant_claims_a_pre_vote_to_the_receiver() {
    let body = Body::PreVoteReply {
        answer: Answer::Granted(None),
    };
    let message = message(4, body, None);
    let claims = claims(&receiver(0), &message);
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
    let raft = receiver(0);
    assert_eq!(raft.claims(&message(4, refusal, None)).count(), 0);
    let vote = Body::Vote {
        last: raft::Position::default(),
    };
    assert_eq!(raft.claims(&message(4, vote, None)).count(), 0);
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

// A link at a random position with random votes, each signed or not, and a change
// signed or not.
fn a_link() -> impl Strategy<Value = Link> {
    let signature = prop::option::of(any::<u8>().prop_map(signature));
    (position(), proof(5), signature).prop_map(|(at, votes, signature)| Link {
        at,
        change: Change {
            voters: voters(&[1, 2, 4]),
            votes,
            signature,
        },
    })
}

proptest! {
    #[test]
    fn sign_gives_each_claim_a_signature_and_keeps_each_it_had(
        term in any::<u64>(),
        body in body(),
        proof in prop::option::of(proof(5)),
        chain in prop::collection::vec(a_link(), 0..3),
    ) {
        let raft = receiver(0);
        // A claim borrows its message, so each is kept as its stand-in signature.
        let signed = |message: &Message| -> Vec<(Signature, Option<Signature>)> {
            raft.claims(message)
                .map(|(claim, had)| (Network::signature(&claim), had))
                .collect()
        };
        let mut message = message(term, body, proof);
        message.chain = chain;
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

    // `claims` gives the links `step` reads, so a link it omits can change freely.
    #[test]
    fn a_link_that_claims_omits_changes_nothing(
        term in any::<u64>(),
        body in body(),
        proof in prop::option::of(proof(5)),
        chain in prop::collection::vec(a_link(), 0..4),
        other in a_link(),
    ) {
        let mut message = message(term, body, proof);
        message.chain = chain;
        let appended = match &message.body {
            Body::Append { entries, .. } => entries
                .iter()
                .filter(|entry| matches!(entry.data, Data::Voters(_)))
                .count(),
            _ => 0,
        };
        let changes = receiver(0)
            .claims(&message)
            .filter(|(claim, _)| matches!(claim, Claim::Change { .. }))
            .count();
        // The receiver's log is empty, so `step` skips the links before the first
        // one above index 0, and reads from there up to link `read`.
        let skipped = message
            .chain
            .iter()
            .position(|link| link.at.index > 0)
            .unwrap_or(message.chain.len());
        let read = skipped + changes - appended;
        prop_assert!(read <= message.chain.len());
        let outcome = |message: Message| {
            let mut raft = receiver(0);
            let result = raft.step(message);
            (result, raft.hard(), raft.role(), raft.leader(), raft.ready())
        };
        let want = outcome(message.clone());
        for omitted in read..message.chain.len() {
            let mut changed = message.clone();
            changed.chain[omitted] = other.clone();
            prop_assert_eq!(outcome(changed), want.clone(), "link {}", omitted);
        }
    }
}
