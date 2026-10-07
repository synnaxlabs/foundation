//! Signs this node's claims and checks the claims of other nodes. A node signs a
//! [`Claim`] with its node key. A grant signs `foundation/grant/1`, the voter, the
//! grant byte, the term, and the candidate. A change signs `foundation/voters/1`, the
//! leader, the term, the index, the incoming voters, and the outgoing voters.

use std::fmt;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use raft::{Claim, Message, Raft, Ready, Signature};
use types::node::{self, PrivateKey, PublicKey};

use crate::bytes::{put_grant, put_key, put_keys, put_position};
use crate::ed25519;

const GRANT: &[u8] = b"foundation/grant/1";
const CHANGE: &[u8] = b"foundation/voters/1";

/// Signs this node's claims with its node key.
pub(crate) struct Signer {
    key: node::Key,
    pair: Ed25519KeyPair,
}

impl Signer {
    /// A signer for the node `key` with its private key.
    pub(crate) fn new(key: node::Key, private: &PrivateKey) -> Self {
        Self {
            key,
            pair: ed25519::pair(private),
        }
    }

    /// Whether `public` checks the claims that this signer signs.
    pub(crate) fn owns(&self, public: PublicKey) -> bool {
        self.pair.public_key().as_ref() == public.to_bytes()
    }

    /// Signs each grant and change in `ready` that has no signature, before the
    /// write and the sends.
    ///
    /// # Panics
    ///
    /// When a grant or a change of another node has no signature: the caller stepped
    /// a message that did not pass [`check`].
    pub(crate) fn sign(&self, ready: &mut Ready) {
        ready.sign(|claim| {
            assert_eq!(
                claim.signer(),
                self.key,
                "invariant: each claim of another node arrives signed"
            );
            Signature(ed25519::sign(&self.pair, &statement(claim)))
        });
    }
}

/// Checks each signature that `raft` reads from `message` against the public keys of
/// the region's members. `public_key` gives the key of a member, and `None` for a
/// node that is not one.
///
/// # Errors
///
/// [`Error`] names the signer of the first claim that fails, in the order of
/// [`Raft::claims`].
///
/// # Panics
///
/// When a claim has no signature. A decoded message gives each claim one.
pub(crate) fn check(
    raft: &Raft,
    message: &Message,
    public_key: impl Fn(node::Key) -> Option<PublicKey>,
) -> Result<(), Error> {
    for (claim, signature) in raft.claims(message) {
        verify(&claim, signature, &public_key)?;
    }
    Ok(())
}

// Checks `signature` against the public key of the signer of `claim`.
fn verify(
    claim: &Claim<'_>,
    signature: Option<Signature>,
    public_key: impl Fn(node::Key) -> Option<PublicKey>,
) -> Result<(), Error> {
    let signer = claim.signer();
    let public = public_key(signer).ok_or(Error::NotMember { signer })?;
    let Signature(bytes) =
        signature.expect("invariant: decode gives each claim a signature");
    if !ed25519::holds(public, &statement(claim), &bytes) {
        return Err(Error::Forged { signer });
    }
    Ok(())
}

/// Why [`check`] refused a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The signer is not a member of the region.
    NotMember {
        /// The node whose signature the claim needs.
        signer: node::Key,
    },
    /// A claim has no signature that holds under the public key of its signer.
    Forged {
        /// The node whose signature the claim needs.
        signer: node::Key,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMember { signer } => {
                write!(f, "node {signer} is not a member of the region")
            }
            Self::Forged { signer } => {
                write!(f, "the claim of node {signer} is forged")
            }
        }
    }
}

impl std::error::Error for Error {}

// The bytes a node signs for `claim`. They name the signer, so members that share a
// key cannot share a signature.
fn statement(claim: &Claim<'_>) -> Vec<u8> {
    match *claim {
        Claim::Grant {
            voter,
            grant,
            term,
            candidate,
        } => {
            let mut bytes = GRANT.to_vec();
            put_key(voter, &mut bytes);
            put_grant(grant, &mut bytes);
            bytes.extend(term.0.to_le_bytes());
            put_key(candidate, &mut bytes);
            bytes
        }
        Claim::Change { leader, at, voters } => {
            let mut bytes = CHANGE.to_vec();
            put_key(leader, &mut bytes);
            put_position(at, &mut bytes);
            put_keys(&voters.incoming, &mut bytes);
            put_keys(&voters.outgoing, &mut bytes);
            bytes
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use proptest::sample::Index;
    use raft::{
        Answer, Body, Change, Config, Data, Grant, Hard, Position, Proof, Start, Term,
        Voters,
    };

    use super::*;
    use crate::bytes::put_optional_proof;
    use crate::common::{
        self, TERM, granted, key, message, public, reply_body, signature, signer,
    };

    fn members(ids: &[u8]) -> impl Fn(node::Key) -> Option<PublicKey> {
        let members: BTreeMap<_, _> =
            ids.iter().map(|&id| (key(id), public(id))).collect();
        move |voter| members.get(&voter).copied()
    }

    // Node `key` of the voters 1, 2 and 3, in term 0 with an empty log.
    fn node(key: node::Key) -> Raft {
        let config = Config {
            key,
            election_ticks: 10,
            heartbeat_ticks: 2,
        };
        let start = Start {
            voters: Voters {
                incoming: [1, 2, 3].map(self::key).into(),
                outgoing: BTreeSet::new(),
            },
            ..Start::default()
        };
        Raft::new(config, start).unwrap()
    }

    // Checks `message` as its receiver, a node of the voters 1, 2 and 3 in term 0.
    fn checked(
        message: &Message,
        public_key: impl Fn(node::Key) -> Option<PublicKey>,
    ) -> Result<(), Error> {
        check(&node(message.to), message, public_key)
    }

    fn proven() -> Message {
        common::proven(1, 2, Body::Heartbeat { commit: 0 })
    }

    // A heartbeat of the term after `TERM` from node 1 to node 2, with the vote of
    // node 1 alone, and a chain of one link: in `TERM`, leader 1 moved the voters
    // from 1, 2 and 3 to 1 alone, elected by all three. The receiver reads the
    // link, which makes the proof a quorum.
    fn chained() -> Message {
        let alone = Voters {
            incoming: [key(1)].into(),
            outgoing: BTreeSet::new(),
        };
        let link = raft::Link {
            at: written(),
            change: signed_change_to(alone),
        };
        let proof = Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [(key(1), None)].into(),
        };
        let mut heartbeat = message(1, 2, Body::Heartbeat { commit: 0 });
        heartbeat.term = Term(TERM.0 + 1);
        heartbeat.proof = Some(proof);
        heartbeat.chain = vec![link];
        let mut ready = Ready {
            messages: vec![heartbeat],
            ..Ready::default()
        };
        signer(1).sign(&mut ready);
        ready.messages.remove(0)
    }

    fn link_of(message: &mut Message) -> &mut Change {
        &mut message.chain[0].change
    }

    #[test]
    fn check_passes_a_chain_whose_votes_and_changes_are_signed() {
        let chained = chained();
        assert_eq!(chained.proof.as_ref().unwrap().voters.len(), 1);
        assert_eq!(checked(&chained, members(&[1, 2, 3])), Ok(()));
        assert_eq!(node(key(2)).step(chained), Ok(()));
    }

    #[test]
    fn check_refuses_a_forged_vote_or_signature_of_a_link_the_receiver_reads() {
        let members = members(&[1, 2, 3]);
        let mut vote = chained();
        let votes = &mut link_of(&mut vote).votes.voters;
        *votes.get_mut(&key(2)).unwrap() = Some(Signature([0; 64]));
        let forged = Err(Error::Forged { signer: key(2) });
        assert_eq!(checked(&vote, &members), forged);
        let mut change = chained();
        link_of(&mut change).signature = Some(Signature([0; 64]));
        let forged = Err(Error::Forged { signer: key(1) });
        assert_eq!(checked(&change, &members), forged);
        let mut both = chained();
        *link_of(&mut both).votes.voters.get_mut(&key(3)).unwrap() =
            Some(Signature([0; 64]));
        link_of(&mut both).signature = Some(Signature([0; 64]));
        let forged = Err(Error::Forged { signer: key(3) });
        assert_eq!(checked(&both, &members), forged);
    }

    // Every proof is a quorum of no voters, so `step` must refuse such a link.
    #[test]
    fn a_link_with_no_voters_does_not_prove_a_leader_with_no_grants() {
        let link = raft::Link {
            at: written(),
            change: signed_change_to(Voters::default()),
        };
        let mut heartbeat = message(1, 2, Body::Heartbeat { commit: 0 });
        heartbeat.term = Term(u64::MAX);
        heartbeat.proof = Some(Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: BTreeMap::new(),
        });
        heartbeat.chain = vec![link];
        assert_eq!(checked(&heartbeat, members(&[1, 2, 3])), Ok(()));
        let mut follower = node(key(2));
        let unproven = raft::Error::Unproven {
            term: Term(u64::MAX),
            from: key(1),
        };
        assert_eq!(follower.step(heartbeat), Err(unproven));
        assert_eq!(follower.term(), Term(0));
    }

    #[test]
    fn check_reads_no_link_when_the_proof_is_a_quorum() {
        let mut message = proven();
        message.chain = chained().chain;
        link_of(&mut message).signature = Some(Signature([0; 64]));
        assert_eq!(checked(&message, members(&[1, 2, 3])), Ok(()));
    }

    #[test]
    #[should_panic(expected = "invariant: decode gives each claim a signature")]
    fn check_panics_on_an_unsigned_link_the_receiver_reads() {
        let mut message = chained();
        link_of(&mut message).signature = None;
        checked(&message, members(&[1, 2, 3])).unwrap();
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
        let claim = |grant| Claim::Grant {
            voter: key(3),
            grant,
            term: Term(7),
            candidate: key(9),
        };
        assert_eq!(statement(&claim(Grant::Vote)), expected);
        assert_eq!(statement(&claim(Grant::PreVote))[34], 0);
    }

    #[test]
    fn the_signed_bytes_of_a_change_are_the_tag_leader_position_and_voters() {
        let mut expected = b"foundation/voters/1".to_vec();
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([7, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([5, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([2, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let voters = Voters {
            incoming: [key(3), key(9)].into(),
            outgoing: [key(3)].into(),
        };
        let claim = Claim::Change {
            leader: key(3),
            at: Position {
                term: Term(7),
                index: 5,
            },
            voters: &voters,
        };
        assert_eq!(statement(&claim), expected);
    }

    // The joint configuration that leader 1 wrote at index 2 of `TERM`.
    fn joint() -> Voters {
        Voters {
            incoming: [key(1), key(2), key(4)].into(),
            outgoing: [key(1), key(2), key(3)].into(),
        }
    }

    fn written() -> Position {
        Position {
            term: TERM,
            index: 2,
        }
    }

    fn signed_change() -> Change {
        signed_change_to(joint())
    }

    // The change to `voters` that leader 1 wrote at `written`, signed.
    fn signed_change_to(voters: Voters) -> Change {
        let Data::Voters(change) = common::change(1, written(), voters).data else {
            unreachable!()
        };
        change
    }

    #[test]
    fn sign_fills_the_votes_and_the_signature_of_a_change_this_node_wrote() {
        let change = signed_change();
        assert_eq!(change.votes, proven().proof.unwrap());
        let joint = joint();
        let claim = Claim::Change {
            leader: key(1),
            at: written(),
            voters: &joint,
        };
        let signature = change.signature;
        assert_eq!(verify(&claim, signature, members(&[1, 2, 3])), Ok(()));
        let unknown = Err(Error::NotMember { signer: key(1) });
        assert_eq!(verify(&claim, signature, members(&[2, 3])), unknown);
    }

    #[test]
    fn a_change_signature_moved_to_another_position_voters_or_leader_is_forged() {
        let signature = signed_change().signature;
        let members = members(&[1, 2, 3]);
        let joint = joint();
        let later = Position {
            index: 3,
            ..written()
        };
        let left = Voters {
            outgoing: BTreeSet::new(),
            ..joint.clone()
        };
        let moved = [
            (key(1), later, &joint),
            (key(1), written(), &left),
            (key(2), written(), &joint),
        ];
        for (leader, at, voters) in moved {
            let claim = Claim::Change { leader, at, voters };
            let forged = Err(Error::Forged { signer: leader });
            assert_eq!(verify(&claim, signature, &members), forged, "{claim:?}");
        }
    }

    #[test]
    #[should_panic(expected = "invariant: each claim of another node arrives signed")]
    fn sign_panics_on_an_unsigned_change_of_another_node() {
        let mut change = signed_change();
        change.votes.candidate = key(2);
        change.signature = None;
        let entry = raft::Entry {
            at: written(),
            data: Data::Voters(change),
        };
        let mut ready = Ready {
            entries: vec![entry],
            ..Ready::default()
        };
        signer(1).sign(&mut ready);
    }

    #[test]
    fn a_signed_grant_passes() {
        for grant in [Grant::PreVote, Grant::Vote] {
            assert_eq!(checked(&granted(2, grant, 1), members(&[1, 2, 3])), Ok(()));
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
            assert_eq!(checked(&message, &members), Ok(()), "leader {leader}");
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
        assert_eq!(checked(&refused, members(&[1, 2])), Ok(()));
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
    #[should_panic(expected = "invariant: each claim of another node arrives signed")]
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
        let unknown = Error::NotMember { signer: key(3) };
        let refused = Err(unknown);
        assert_eq!(checked(&proven(), members(&[1, 2])), refused);
        assert_eq!(
            unknown.to_string(),
            format!("node {} is not a member of the region", key(3))
        );
        let reply = granted(3, Grant::Vote, 1);
        assert_eq!(checked(&reply, members(&[1, 2])), refused);
    }

    // An append of the change of leader 1 at index 2 of `TERM`, proven by 1.
    fn change_append() -> Message {
        let append = Body::Append {
            prev: Position::default(),
            entries: vec![raft::Entry {
                at: written(),
                data: Data::Voters(signed_change()),
            }],
            commit: 0,
        };
        common::proven(1, 2, append)
    }

    fn change_of(message: &mut Message) -> &mut Change {
        let Body::Append { entries, .. } = &mut message.body else {
            unreachable!()
        };
        let Data::Voters(change) = &mut entries[0].data else {
            unreachable!()
        };
        change
    }

    #[test]
    fn check_refuses_a_forged_vote_or_signature_of_a_change_an_append_carries() {
        let members = members(&[1, 2, 3]);
        assert_eq!(checked(&change_append(), &members), Ok(()));
        let mut zeroed = change_append();
        change_of(&mut zeroed).signature = Some(Signature([0; 64]));
        let forged = Err(Error::Forged { signer: key(1) });
        assert_eq!(checked(&zeroed, &members), forged);
        let mut vote = change_append();
        let votes = &mut change_of(&mut vote).votes.voters;
        *votes.get_mut(&key(2)).unwrap() = Some(Signature([0; 64]));
        let forged = Err(Error::Forged { signer: key(2) });
        assert_eq!(checked(&vote, &members), forged);
    }

    #[test]
    #[should_panic(expected = "invariant: decode gives each claim a signature")]
    fn check_panics_on_an_unsigned_change_an_append_carries() {
        let mut message = change_append();
        change_of(&mut message).signature = None;
        checked(&message, members(&[1, 2, 3])).unwrap();
    }

    #[test]
    fn check_refuses_a_changed_signature_byte() {
        let mut message = proven();
        voter(&mut message, 2).as_mut().unwrap().0[63] ^= 1;
        let forged = Error::Forged { signer: key(2) };
        assert_eq!(checked(&message, members(&[1, 2, 3])), Err(forged));
        assert_eq!(
            forged.to_string(),
            format!("the claim of node {} is forged", key(2))
        );
    }

    #[test]
    #[should_panic(expected = "invariant: decode gives each claim a signature")]
    fn check_panics_on_an_unsigned_entry() {
        let mut message = proven();
        *voter(&mut message, 3) = None;
        checked(&message, members(&[1, 2, 3])).unwrap();
    }

    #[test]
    fn check_refuses_a_signature_of_another_voter_with_the_same_key() {
        let two = members(&[1, 2]);
        let members = |voter| {
            if voter == key(3) {
                Some(public(2))
            } else {
                two(voter)
            }
        };
        let mut message = proven();
        *voter(&mut message, 3) = Some(signature(2, Grant::Vote, 1));
        let refused = Err(Error::Forged { signer: key(3) });
        assert_eq!(checked(&message, members), refused);
    }

    #[test]
    fn check_names_the_first_voter_that_fails() {
        let mut message = proven();
        *voter(&mut message, 3) = Some(signature(2, Grant::Vote, 1));
        let refused = Err(Error::NotMember { signer: key(2) });
        assert_eq!(checked(&message, members(&[1, 3])), refused);
    }

    #[test]
    fn check_refuses_a_proof_signature_moved_to_another_field() {
        let members = members(&[1, 2, 3, 4]);
        let forged = Err(Error::Forged { signer: key(1) });
        let mut later = proven();
        later.term = Term(TERM.0 + 1);
        assert_eq!(checked(&later, &members), forged);
        let mut other = proven();
        other.proof.as_mut().unwrap().candidate = key(4);
        assert_eq!(checked(&other, &members), forged);
        let mut pre_vote = proven();
        pre_vote.proof.as_mut().unwrap().grant = Grant::PreVote;
        assert_eq!(checked(&pre_vote, &members), forged);
    }

    #[test]
    fn check_refuses_a_grant_signature_moved_to_another_field() {
        let members = members(&[1, 2, 3]);
        let forged = Err(Error::Forged { signer: key(2) });
        let mut later = granted(2, Grant::Vote, 1);
        later.term = Term(TERM.0 + 1);
        assert_eq!(checked(&later, &members), forged);
        let mut other = granted(2, Grant::Vote, 1);
        other.to = key(3);
        assert_eq!(checked(&other, &members), forged);
        let mut pre_vote = granted(2, Grant::Vote, 1);
        let answer = Answer::Granted(Some(signature(2, Grant::Vote, 1)));
        pre_vote.body = reply_body(Grant::PreVote, answer);
        assert_eq!(checked(&pre_vote, &members), forged);
    }

    #[test]
    fn check_refuses_a_grant_that_another_node_signed() {
        let mut moved = granted(2, Grant::Vote, 1);
        moved.from = key(3);
        let refused = Err(Error::Forged { signer: key(3) });
        assert_eq!(checked(&moved, members(&[1, 2, 3])), refused);
    }

    // Three voters that sign each `Ready`, send its byte form, and check each
    // message before `step`, as the driver does.
    struct Group {
        nodes: Vec<(Raft, Signer)>,
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
            Self {
                nodes: [1, 2, 3].map(|id| (node(key(id)), signer(id))).into(),
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
            let (node, _) = self
                .nodes
                .iter_mut()
                .find(|(node, _)| node.key() == message.to)
                .unwrap();
            let members = members(&[1, 2, 3]);
            assert_eq!(check(node, &message, members), Ok(()), "{message:?}");
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
