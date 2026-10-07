//! Signs this node's grants and checks the grants of other nodes. A voter signs a
//! [`Claim`] with its node key: `foundation/grant/1`, the voter, the grant byte, the
//! term, and the candidate.

use std::collections::BTreeMap;
use std::fmt;

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use raft::{Claim, Message, Ready, Signature};
use types::node::{self, PrivateKey, PublicKey};

use crate::bytes::{put_grant, put_key};

const TAG: &[u8] = b"foundation/grant/1";

/// Signs this node's grants with its node key.
pub(crate) struct Signer {
    key: node::Key,
    pair: Ed25519KeyPair,
}

impl Signer {
    /// A signer for the node `key` with its private key.
    pub(crate) fn new(key: node::Key, private: &PrivateKey) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&private.0)
            .expect("invariant: any 32 bytes are an Ed25519 private key");
        Self { key, pair }
    }

    /// Signs each grant in `ready` that has no signature, before the write and the
    /// sends.
    ///
    /// # Panics
    ///
    /// When a grant of another node has no signature: the caller stepped a message
    /// that did not pass [`check`].
    pub(crate) fn sign(&self, ready: &mut Ready) {
        ready.sign(|claim| {
            assert_eq!(
                claim.voter, self.key,
                "invariant: each message passed `check` before `step`"
            );
            let signature = self.pair.sign(&statement(claim));
            Signature(
                signature
                    .as_ref()
                    .try_into()
                    .expect("invariant: an Ed25519 signature is 64 bytes"),
            )
        });
    }
}

/// Checks each signature that `message` carries against the public keys of the
/// region's members.
///
/// # Errors
///
/// [`Error`] names the first voter that fails, in the order of
/// [`Message::claims`].
///
/// # Panics
///
/// When a grant has no signature. A decoded message gives each grant one.
pub(crate) fn check(
    message: &Message,
    members: &BTreeMap<node::Key, PublicKey>,
) -> Result<(), Error> {
    for (claim, signature) in message.claims() {
        let voter = claim.voter;
        let public = members.get(&voter).ok_or(Error::NotMember { voter })?;
        let Signature(bytes) =
            signature.expect("invariant: decode gives each grant a signature");
        let public = UnparsedPublicKey::new(&ED25519, public.to_bytes());
        if public.verify(&statement(&claim), &bytes).is_err() {
            return Err(Error::Forged { voter });
        }
    }
    Ok(())
}

/// Why [`check`] refused a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The voter is not a member of the region.
    NotMember {
        /// The voter.
        voter: node::Key,
    },
    /// The voter's grant has no signature that holds under its public key.
    Forged {
        /// The voter.
        voter: node::Key,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMember { voter } => {
                write!(f, "voter {voter} is not a member of the region")
            }
            Self::Forged { voter } => write!(f, "the grant of voter {voter} is forged"),
        }
    }
}

impl std::error::Error for Error {}

// The bytes a voter signs for `claim`. They name the voter, so members that share a
// key cannot share a signature.
fn statement(claim: &Claim) -> Vec<u8> {
    let mut bytes = TAG.to_vec();
    put_key(claim.voter, &mut bytes);
    put_grant(claim.grant, &mut bytes);
    bytes.extend(claim.term.0.to_le_bytes());
    put_key(claim.candidate, &mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;
    use proptest::sample::Index;
    use raft::{Answer, Body, Config, Data, Grant, Hard, Raft, Start, Term, Voters};

    use super::*;
    use crate::bytes::put_optional_proof;
    use crate::common::{
        self, TERM, granted, key, members, message, public, reply_body, signature,
        signer,
    };

    fn proven() -> Message {
        common::proven(1, 2, Body::Heartbeat { commit: 0 })
    }

    fn voter(message: &mut Message, id: u8) -> &mut Option<Signature> {
        let proof = message.proof.as_mut().unwrap();
        proof.voters.get_mut(&key(id)).unwrap()
    }

    #[test]
    fn the_signed_bytes_are_the_tag_voter_grant_term_and_candidate() {
        let mut expected = b"foundation/grant/1".to_vec();
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.push(1);
        expected.extend([7, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let claim = Claim {
            voter: key(3),
            grant: Grant::Vote,
            term: Term(7),
            candidate: key(9),
        };
        assert_eq!(statement(&claim), expected);
        let pre_vote = Claim {
            grant: Grant::PreVote,
            ..claim
        };
        assert_eq!(statement(&pre_vote)[34], 0);
    }

    #[test]
    fn a_signed_grant_passes() {
        for grant in [Grant::PreVote, Grant::Vote] {
            assert_eq!(check(&granted(2, grant, 1), &members(&[1, 2, 3])), Ok(()));
        }
    }

    #[test]
    fn a_signed_proof_of_each_voter_passes() {
        let members = members(&[1, 2, 3]);
        for leader in 1..=3 {
            let to = leader % 3 + 1;
            let body = Body::Heartbeat {
                commit: u64::from(leader),
            };
            let message = common::proven(leader, to, body.clone());
            assert_eq!((message.from, message.to), (key(leader), key(to)));
            assert_eq!(message.body, body);
            assert_eq!(check(&message, &members), Ok(()), "leader {leader}");
        }
    }

    #[test]
    fn a_refusal_carries_no_signature() {
        let refused = message(2, 1, reply_body(Grant::Vote, Answer::Refused));
        let mut ready = Ready {
            messages: vec![refused.clone()],
            ..Ready::default()
        };
        signer(2).sign(&mut ready);
        assert_eq!(ready.messages, std::slice::from_ref(&refused));
        assert_eq!(check(&refused, &members(&[1, 2])), Ok(()));
    }

    #[test]
    fn sign_fills_the_hard_proof_with_its_term() {
        let mut proof = proven().proof.unwrap();
        proof.voters.insert(key(1), None);
        let hard = Hard {
            term: TERM,
            proof: Some(proof),
            ..Hard::default()
        };
        let mut ready = Ready {
            hard: Some(hard),
            ..Ready::default()
        };
        signer(1).sign(&mut ready);
        assert_eq!(ready.hard.unwrap().proof, proven().proof);
    }

    #[test]
    fn sign_keeps_the_signatures_of_other_nodes() {
        let mut ready = Ready {
            messages: vec![proven()],
            ..Ready::default()
        };
        signer(1).sign(&mut ready);
        assert_eq!(ready.messages, [proven()]);
    }

    #[test]
    #[should_panic(expected = "invariant: each message passed `check` before `step`")]
    fn sign_panics_on_an_unsigned_entry_of_another_node() {
        let mut message = proven();
        *voter(&mut message, 2) = None;
        let mut ready = Ready {
            messages: vec![message],
            ..Ready::default()
        };
        signer(1).sign(&mut ready);
    }

    #[test]
    fn check_refuses_a_voter_that_is_not_a_member() {
        let unknown = Error::NotMember { voter: key(3) };
        let refused = Err(unknown);
        assert_eq!(check(&proven(), &members(&[1, 2])), refused);
        assert_eq!(
            unknown.to_string(),
            format!("voter {} is not a member of the region", key(3))
        );
        let reply = granted(3, Grant::Vote, 1);
        assert_eq!(check(&reply, &members(&[1, 2])), refused);
    }

    #[test]
    fn check_refuses_a_changed_signature_byte() {
        let mut message = proven();
        voter(&mut message, 2).as_mut().unwrap().0[63] ^= 1;
        let forged = Error::Forged { voter: key(2) };
        assert_eq!(check(&message, &members(&[1, 2, 3])), Err(forged));
        assert_eq!(
            forged.to_string(),
            format!("the grant of voter {} is forged", key(2))
        );
    }

    #[test]
    #[should_panic(expected = "invariant: decode gives each grant a signature")]
    fn check_panics_on_an_unsigned_entry() {
        let mut message = proven();
        *voter(&mut message, 3) = None;
        check(&message, &members(&[1, 2, 3])).unwrap();
    }

    #[test]
    fn check_refuses_a_signature_of_another_voter_with_the_same_key() {
        let mut members = members(&[1, 2]);
        members.insert(key(3), public(2));
        let mut message = proven();
        *voter(&mut message, 3) = Some(signature(2, Grant::Vote, 1));
        let refused = Err(Error::Forged { voter: key(3) });
        assert_eq!(check(&message, &members), refused);
    }

    #[test]
    fn check_names_the_first_voter_that_fails() {
        let mut message = proven();
        *voter(&mut message, 3) = Some(signature(2, Grant::Vote, 1));
        let refused = Err(Error::NotMember { voter: key(2) });
        assert_eq!(check(&message, &members(&[1, 3])), refused);
    }

    #[test]
    fn check_refuses_a_proof_signature_moved_to_another_field() {
        let members = members(&[1, 2, 3, 4]);
        let forged = Err(Error::Forged { voter: key(1) });
        let mut later = proven();
        later.term = Term(TERM.0 + 1);
        assert_eq!(check(&later, &members), forged);
        let mut other = proven();
        other.proof.as_mut().unwrap().candidate = key(4);
        assert_eq!(check(&other, &members), forged);
        let mut pre_vote = proven();
        pre_vote.proof.as_mut().unwrap().grant = Grant::PreVote;
        assert_eq!(check(&pre_vote, &members), forged);
    }

    #[test]
    fn check_refuses_a_grant_signature_moved_to_another_field() {
        let members = members(&[1, 2, 3]);
        let forged = Err(Error::Forged { voter: key(2) });
        let mut later = granted(2, Grant::Vote, 1);
        later.term = Term(TERM.0 + 1);
        assert_eq!(check(&later, &members), forged);
        let mut other = granted(2, Grant::Vote, 1);
        other.to = key(3);
        assert_eq!(check(&other, &members), forged);
        let mut pre_vote = granted(2, Grant::Vote, 1);
        let answer = Answer::Granted(Some(signature(2, Grant::Vote, 1)));
        pre_vote.body = reply_body(Grant::PreVote, answer);
        assert_eq!(check(&pre_vote, &members), forged);
    }

    #[test]
    fn check_refuses_a_grant_that_another_node_signed() {
        let mut moved = granted(2, Grant::Vote, 1);
        moved.from = key(3);
        let refused = Err(Error::Forged { voter: key(3) });
        assert_eq!(check(&moved, &members(&[1, 2, 3])), refused);
    }

    // Three voters that sign each `Ready`, send its byte form, and check each
    // message before `step`, as the driver does.
    struct Group {
        nodes: Vec<(Raft, Signer)>,
        members: BTreeMap<node::Key, PublicKey>,
        flight: Vec<Vec<u8>>,
        committed: Vec<Vec<Data>>,
    }

    #[derive(Clone, Debug)]
    enum Action {
        Tick(Index, u64),
        Deliver(Index),
        Drop(Index),
        Propose,
    }

    impl Group {
        fn new() -> Self {
            let voters = Voters {
                incoming: [1, 2, 3].map(key).into(),
                outgoing: BTreeSet::new(),
            };
            let node = |id| {
                let config = Config {
                    key: key(id),
                    election_ticks: 10,
                    heartbeat_ticks: 2,
                };
                let start = Start {
                    voters: voters.clone(),
                    ..Start::default()
                };
                (Raft::new(config, start).unwrap(), signer(id))
            };
            Self {
                nodes: [1, 2, 3].map(node).into(),
                members: members(&[1, 2, 3]),
                flight: Vec::new(),
                committed: vec![Vec::new(); 3],
            }
        }

        fn apply(&mut self, action: &Action) {
            match *action {
                Action::Tick(at, random) => {
                    let len = self.nodes.len();
                    self.nodes[at.index(len)].0.tick(random);
                }
                Action::Deliver(at) if !self.flight.is_empty() => {
                    let bytes = self.flight.remove(at.index(self.flight.len()));
                    self.deliver(&bytes);
                }
                Action::Drop(at) if !self.flight.is_empty() => {
                    self.flight.remove(at.index(self.flight.len()));
                }
                Action::Propose => self.propose(),
                Action::Deliver(_) | Action::Drop(_) => {}
            }
            self.collect();
        }

        fn propose(&mut self) {
            for (node, _) in &mut self.nodes {
                if node.role() == raft::Role::Leader {
                    node.propose(b"end".to_vec()).unwrap();
                }
            }
        }

        fn deliver(&mut self, bytes: &[u8]) {
            let Some(crate::message::Message::Raft(message)) =
                crate::message::Message::decode(bytes)
            else {
                panic!("{bytes:?} is not a raft message");
            };
            assert_eq!(check(&message, &self.members), Ok(()), "{message:?}");
            let (node, _) = self
                .nodes
                .iter_mut()
                .find(|(node, _)| node.key() == message.to)
                .unwrap();
            node.step(message).unwrap();
        }

        fn collect(&mut self) {
            for ((node, signer), committed) in
                self.nodes.iter_mut().zip(&mut self.committed)
            {
                let mut ready = node.ready();
                signer.sign(&mut ready);
                // The byte form panics on an unsigned entry.
                if let Some(hard) = &ready.hard {
                    put_optional_proof(hard.proof.as_ref(), &mut Vec::new());
                }
                committed.extend(ready.committed.into_iter().map(|entry| entry.data));
                for message in ready.messages {
                    let message = crate::message::Message::Raft(message);
                    self.flight.push(message.encode());
                }
            }
        }

        // Mends the network, then runs until each node applies an entry. Panics when
        // the group does not get there.
        fn settle(&mut self) {
            let end = Data::Bytes(b"end".to_vec());
            for round in 0_u64..2000 {
                if self.committed.iter().all(|data| data.contains(&end)) {
                    return;
                }
                for at in 0..self.nodes.len() {
                    let random = round.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                        ^ u64::try_from(at).unwrap();
                    self.nodes[at].0.tick(random);
                }
                self.collect();
                while !self.flight.is_empty() {
                    let bytes = self.flight.remove(0);
                    self.deliver(&bytes);
                    self.collect();
                }
                self.propose();
                self.collect();
            }
            panic!("the group applies no entry: {:?}", self.committed);
        }
    }

    fn action() -> impl Strategy<Value = Action> {
        prop_oneof![
            3 => (any::<Index>(), any::<u64>())
                .prop_map(|(at, random)| Action::Tick(at, random)),
            6 => any::<Index>().prop_map(Action::Deliver),
            1 => any::<Index>().prop_map(Action::Drop),
            1 => Just(Action::Propose),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // Each message of an honest node passes `check`, through drops and
        // reorders, and the group still elects a leader and commits.
        #[test]
        fn a_group_that_signs_and_checks_commits(
            actions in prop::collection::vec(action(), 0..400),
        ) {
            let mut group = Group::new();
            for action in &actions {
                group.apply(action);
            }
            group.settle();
        }
    }
}
