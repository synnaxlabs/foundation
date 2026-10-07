//! The byte form of what two nodes of a region say on a mesh stream.
//!
//! A message has one byte form: [`Message::decode`] takes only what
//! [`Message::encode`] gives. The stream's header carries the format version.

use raft::{Answer, Body, Position, Term};
use types::node;

use crate::bytes::{
    put_key, put_optional_proof, put_position, put_signature, take, take_bool,
    take_key, take_position, take_proof, take_signature,
};
use crate::change::Change;
use crate::entry;

const RAFT: u8 = 1;
const PROPOSE: u8 = 2;
const PROPOSED: u8 = 3;
const NOT_LEADER: u8 = 4;
const NOT_LEADER_WITH_LEADER: u8 = 5;

const PRE_VOTE: u8 = 1;
const PRE_VOTE_REPLY: u8 = 2;
const VOTE: u8 = 3;
const VOTE_REPLY: u8 = 4;
const HEARTBEAT: u8 = 5;
const HEARTBEAT_REPLY: u8 = 6;
const APPEND: u8 = 7;
const APPEND_REPLY: u8 = 8;
const APPEND_REJECT: u8 = 9;

/// One message between two nodes of a region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    /// A message of the `raft` group.
    Raft(raft::Message),
    /// Asks the leader to propose `change`. It is the one message of its stream, and
    /// the answer comes on the reply half. A sender that gets no answer sends the
    /// change again on a new stream, so a change applies at least one time.
    Propose {
        /// The change.
        change: Change,
    },
    /// Answers a [`Message::Propose`] that the receiver proposed.
    Proposed {
        /// The position of the entry, which is on the receiver's disk. A new leader
        /// can replace it.
        at: Position,
    },
    /// Answers a [`Message::Propose`] whose change the receiver holds in no entry,
    /// because it does not lead.
    NotLeader {
        /// The leader that the receiver knows.
        leader: Option<node::Key>,
    },
}

impl Message {
    /// The byte form of the message.
    ///
    /// # Panics
    ///
    /// When a grant or a proof entry has no signature: the caller signs them first.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Raft(message) => {
                out.push(RAFT);
                put_key(message.from, &mut out);
                put_key(message.to, &mut out);
                out.extend(message.term.0.to_le_bytes());
                put_optional_proof(message.proof.as_ref(), &mut out);
                body(&message.body, &mut out);
            }
            Self::Propose { change } => {
                out.push(PROPOSE);
                change.encode(&mut out);
            }
            Self::Proposed { at } => {
                out.push(PROPOSED);
                put_position(*at, &mut out);
            }
            Self::NotLeader { leader } => {
                out.push(leader.map_or(NOT_LEADER, |_| NOT_LEADER_WITH_LEADER));
                if let Some(leader) = leader {
                    put_key(*leader, &mut out);
                }
            }
        }
        out
    }

    /// The message that `bytes` is the byte form of, or `None` when it is the byte
    /// form of no message.
    pub(crate) fn decode(mut bytes: &[u8]) -> Option<Self> {
        let bytes = &mut bytes;
        let message = match u8::from_le_bytes(take(bytes)?) {
            RAFT => {
                let from = take_key(bytes)?;
                let to = take_key(bytes)?;
                let term = Term(u64::from_le_bytes(take(bytes)?));
                let proof = if take_bool(bytes)? {
                    Some(take_proof(bytes)?)
                } else {
                    None
                };
                Self::Raft(raft::Message {
                    from,
                    to,
                    term,
                    body: take_body(bytes)?,
                    proof,
                })
            }
            PROPOSE => Self::Propose {
                change: Change::decode(std::mem::take(bytes)).ok()?,
            },
            PROPOSED => Self::Proposed {
                at: take_position(bytes)?,
            },
            NOT_LEADER => Self::NotLeader { leader: None },
            NOT_LEADER_WITH_LEADER => Self::NotLeader {
                leader: Some(take_key(bytes)?),
            },
            _ => return None,
        };
        bytes.is_empty().then_some(message)
    }
}

fn body(body: &Body, out: &mut Vec<u8>) {
    match body {
        Body::PreVote { last } => {
            out.push(PRE_VOTE);
            put_position(*last, out);
        }
        Body::PreVoteReply { answer } => {
            out.push(PRE_VOTE_REPLY);
            put_answer(*answer, out);
        }
        Body::Vote { last } => {
            out.push(VOTE);
            put_position(*last, out);
        }
        Body::VoteReply { answer } => {
            out.push(VOTE_REPLY);
            put_answer(*answer, out);
        }
        Body::Heartbeat { commit } => {
            out.push(HEARTBEAT);
            out.extend(commit.to_le_bytes());
        }
        Body::HeartbeatReply => out.push(HEARTBEAT_REPLY),
        Body::Append {
            prev,
            entries,
            commit,
        } => {
            out.push(APPEND);
            put_position(*prev, out);
            out.extend(commit.to_le_bytes());
            for entry in entries {
                entry::encode(entry, out);
            }
        }
        Body::AppendReply { last } => {
            out.push(APPEND_REPLY);
            out.extend(last.to_le_bytes());
        }
        Body::AppendReject { hint } => {
            out.push(APPEND_REJECT);
            out.extend(hint.to_le_bytes());
        }
    }
}

// The entries of an `Append` go to the end of the message.
fn take_body(bytes: &mut &[u8]) -> Option<Body> {
    let body = match u8::from_le_bytes(take(bytes)?) {
        PRE_VOTE => Body::PreVote {
            last: take_position(bytes)?,
        },
        PRE_VOTE_REPLY => Body::PreVoteReply {
            answer: take_answer(bytes)?,
        },
        VOTE => Body::Vote {
            last: take_position(bytes)?,
        },
        VOTE_REPLY => Body::VoteReply {
            answer: take_answer(bytes)?,
        },
        HEARTBEAT => Body::Heartbeat {
            commit: u64::from_le_bytes(take(bytes)?),
        },
        HEARTBEAT_REPLY => Body::HeartbeatReply,
        APPEND => {
            let prev = take_position(bytes)?;
            let commit = u64::from_le_bytes(take(bytes)?);
            let mut entries = Vec::new();
            while !bytes.is_empty() {
                entries.push(entry::decode(bytes)?);
            }
            Body::Append {
                prev,
                entries,
                commit,
            }
        }
        APPEND_REPLY => Body::AppendReply {
            last: u64::from_le_bytes(take(bytes)?),
        },
        APPEND_REJECT => Body::AppendReject {
            hint: u64::from_le_bytes(take(bytes)?),
        },
        _ => return None,
    };
    Some(body)
}

const REFUSED: u8 = 0;
const GRANTED: u8 = 1;

// A refusal is its byte alone. A grant is its byte, then its signature.
fn put_answer(answer: Answer, out: &mut Vec<u8>) {
    match answer {
        Answer::Refused => out.push(REFUSED),
        Answer::Granted(signature) => {
            out.push(GRANTED);
            put_signature(signature, out);
        }
    }
}

fn take_answer(bytes: &mut &[u8]) -> Option<Answer> {
    match u8::from_le_bytes(take(bytes)?) {
        REFUSED => Some(Answer::Refused),
        GRANTED => Some(Answer::Granted(Some(take_signature(bytes)?))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;
    use raft::{Data, Entry, Grant, Proof, Signature, Voters};
    use types::channel;

    use super::*;

    fn node(bits: u128) -> node::Key {
        node::Key::from_u128(bits)
    }

    fn at(term: u64, index: u64) -> Position {
        Position {
            term: Term(term),
            index,
        }
    }

    fn a_position() -> impl Strategy<Value = Position> {
        (any::<u64>(), any::<u64>()).prop_map(|(term, index)| at(term, index))
    }

    fn keys() -> impl Strategy<Value = BTreeSet<node::Key>> {
        prop::collection::btree_set(any::<u128>().prop_map(node), 0..4)
    }

    fn a_signature() -> impl Strategy<Value = Signature> {
        prop::array::uniform(any::<u8>()).prop_map(Signature)
    }

    fn an_answer() -> impl Strategy<Value = Answer> {
        prop_oneof![
            Just(Answer::Refused),
            a_signature().prop_map(|signature| Answer::Granted(Some(signature))),
        ]
    }

    fn an_entry() -> impl Strategy<Value = Entry> {
        let data = prop_oneof![
            Just(Data::Empty),
            prop::collection::vec(any::<u8>(), 0..48).prop_map(Data::Bytes),
            (keys(), keys(), a_proof(), a_signature()).prop_map(
                |(incoming, outgoing, votes, signature)| {
                    Data::Voters(raft::Change {
                        voters: Voters { incoming, outgoing },
                        votes,
                        signature: Some(signature),
                    })
                },
            ),
        ];
        (a_position(), data).prop_map(|(at, data)| Entry { at, data })
    }

    fn a_body() -> impl Strategy<Value = Body> {
        let entries = prop::collection::vec(an_entry(), 0..4);
        prop_oneof![
            a_position().prop_map(|last| Body::PreVote { last }),
            an_answer().prop_map(|answer| Body::PreVoteReply { answer }),
            a_position().prop_map(|last| Body::Vote { last }),
            an_answer().prop_map(|answer| Body::VoteReply { answer }),
            any::<u64>().prop_map(|commit| Body::Heartbeat { commit }),
            Just(Body::HeartbeatReply),
            (a_position(), entries, any::<u64>()).prop_map(
                |(prev, entries, commit)| {
                    Body::Append {
                        prev,
                        entries,
                        commit,
                    }
                }
            ),
            any::<u64>().prop_map(|last| Body::AppendReply { last }),
            any::<u64>().prop_map(|hint| Body::AppendReject { hint }),
        ]
    }

    fn a_proof() -> impl Strategy<Value = Proof> {
        let grant = any::<bool>()
            .prop_map(|vote| if vote { Grant::Vote } else { Grant::PreVote });
        let voters = prop::collection::btree_map(
            any::<u128>().prop_map(node),
            a_signature().prop_map(Some),
            0..4,
        );
        (grant, any::<u128>(), voters).prop_map(|(grant, candidate, voters)| Proof {
            grant,
            candidate: node(candidate),
            voters,
        })
    }

    fn a_message() -> impl Strategy<Value = Message> {
        let fields = (
            any::<u128>(),
            any::<u128>(),
            any::<u64>(),
            a_body(),
            prop::option::of(a_proof()),
        );
        let raft = fields.prop_map(|(from, to, term, body, proof)| {
            Message::Raft(raft::Message {
                from: node(from),
                to: node(to),
                term: Term(term),
                body,
                proof,
            })
        });
        let propose =
            (any::<u128>(), any::<u128>()).prop_map(|(index, home)| Message::Propose {
                change: Change::Home {
                    index: channel::Key::from_u128(index),
                    home: node(home),
                },
            });
        let proposed = a_position().prop_map(|at| Message::Proposed { at });
        let not_leader =
            prop::option::of(any::<u128>()).prop_map(|leader| Message::NotLeader {
                leader: leader.map(node),
            });
        prop_oneof![4 => raft, 1 => propose, 1 => proposed, 1 => not_leader]
    }

    fn le(number: u64) -> [u8; 8] {
        number.to_le_bytes()
    }

    fn key(low: u8) -> Vec<u8> {
        let mut key = vec![low];
        key.extend([0; 15]);
        key
    }

    fn signature(byte: u8) -> Signature {
        Signature([byte; 64])
    }

    // An append of one change to the incoming voters `keys`, with no outgoing voter.
    fn change_append(keys: [u128; 2]) -> Vec<u8> {
        raft(Body::Append {
            prev: at(0, 0),
            entries: vec![Entry {
                at: at(3, 1),
                data: Data::Voters(raft::Change {
                    voters: Voters {
                        incoming: keys.map(node).into(),
                        outgoing: BTreeSet::new(),
                    },
                    votes: Proof {
                        grant: Grant::Vote,
                        candidate: node(1),
                        voters: [(node(1), Some(signature(1)))].into(),
                    },
                    signature: Some(signature(2)),
                }),
            }],
            commit: 0,
        })
        .encode()
    }

    fn raft(body: Body) -> Message {
        Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body,
            proof: None,
        })
    }

    /// The bytes of [`raft`] before the body, with no proof.
    fn head() -> Vec<u8> {
        [&[RAFT][..], &key(1), &key(2), &le(3), &[0]].concat()
    }

    #[test]
    fn each_body_has_a_fixed_byte_form() {
        let cases: [(Body, &[&[u8]]); 8] = [
            (Body::PreVote { last: at(2, 4) }, &[&[1], &le(2), &le(4)]),
            (
                Body::PreVoteReply {
                    answer: Answer::Granted(Some(signature(7))),
                },
                &[&[2, 1], &[7; 64]],
            ),
            (Body::Vote { last: at(2, 4) }, &[&[3], &le(2), &le(4)]),
            (
                Body::VoteReply {
                    answer: Answer::Refused,
                },
                &[&[4, 0]],
            ),
            (Body::Heartbeat { commit: 6 }, &[&[5], &le(6)]),
            (Body::HeartbeatReply, &[&[6]]),
            (Body::AppendReply { last: 8 }, &[&[8], &le(8)]),
            (Body::AppendReject { hint: 9 }, &[&[9], &le(9)]),
        ];
        for (body, tail) in cases {
            let message = raft(body);
            let expected = [head(), tail.concat()].concat();
            assert_eq!(message.encode(), expected, "{message:?}");
            assert_eq!(Message::decode(&expected), Some(message));
        }
    }

    #[test]
    fn an_append_has_a_fixed_byte_form() {
        let message = raft(Body::Append {
            prev: at(2, 4),
            entries: vec![Entry {
                at: at(3, 5),
                data: Data::Bytes(vec![0xAA, 0xBB]),
            }],
            commit: 4,
        });
        let prev = [le(2), le(4)].concat();
        let commit = le(4);
        let entry = [&le(3)[..], &le(5), &[1], &le(2), &[0xAA, 0xBB]].concat();
        let expected = [&head()[..], &[7], &prev, &commit, &entry].concat();
        assert_eq!(message.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(message));
    }

    #[test]
    fn a_proof_has_a_fixed_byte_form() {
        let message = Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body: Body::Heartbeat { commit: 6 },
            proof: Some(Proof {
                grant: Grant::Vote,
                candidate: node(1),
                voters: [(node(1), Some(signature(5))), (node(4), Some(signature(6)))]
                    .into(),
            }),
        });
        let voters = [&le(2)[..], &key(1), &[5; 64], &key(4), &[6; 64]].concat();
        let proof = [&[1, 1][..], &key(1), &voters].concat();
        let head = &head()[..head().len() - 1];
        let expected = [head, &proof, &[5], &le(6)].concat();
        assert_eq!(message.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(message));
    }

    #[test]
    fn a_proposal_and_its_answers_have_a_fixed_byte_form() {
        let propose = Message::Propose {
            change: Change::Home {
                index: channel::Key::from_u128(7),
                home: node(8),
            },
        };
        let known = Message::NotLeader {
            leader: Some(node(7)),
        };
        let cases: [(Message, &[&[u8]]); 4] = [
            (propose, &[&[2, 1], &key(7), &key(8)]),
            (Message::Proposed { at: at(2, 4) }, &[&[3], &le(2), &le(4)]),
            (Message::NotLeader { leader: None }, &[&[4]]),
            (known, &[&[5], &key(7)]),
        ];
        for (message, expected) in cases {
            let expected = expected.concat();
            assert_eq!(message.encode(), expected, "{message:?}");
            assert_eq!(Message::decode(&expected), Some(message));
        }
    }

    #[test]
    fn decode_refuses_what_is_not_a_message() {
        let heartbeat = raft(Body::HeartbeatReply).encode();
        let granted = raft(Body::VoteReply {
            answer: Answer::Granted(Some(signature(7))),
        })
        .encode();
        let mut answer = granted.clone();
        answer[head().len() + 1] = 2;
        let unsigned = [&head()[..], &[4, 1]].concat();
        let mut body = heartbeat.clone();
        *body.last_mut().unwrap() = 10;
        let mut tail = heartbeat.clone();
        tail.push(0);
        let proof_at = head().len() - 1;
        let mut presence = heartbeat.clone();
        presence[proof_at] = 2;
        let grant = [&head()[..proof_at], &[1, 2], &key(1), &le(0), &[6]].concat();
        let proven = |voters: [u8; 2]| {
            let voters = voters.map(|voter| [key(voter), vec![voter; 64]].concat());
            let proof = [&[1, 1][..], &key(1), &le(2), &voters.concat()].concat();
            [&head()[..proof_at], &proof, &[6]].concat()
        };
        // Two keys that fall, and one key twice: the count of the outgoing set, the
        // votes of one voter, and the signature follow the two incoming keys.
        let after = 8 + (1 + 16 + 8 + 16 + 64) + 64;
        let mut falling = change_append([1, 2]);
        let len = falling.len();
        falling.swap(len - after - 32, len - after - 16);
        let mut twice = change_append([1, 2]);
        twice.swap(len - after - 32, len - after - 16);
        twice[len - after - 32] = 2;
        twice[len - after - 16] = 2;
        let cases: [(&str, &[u8]); 13] = [
            ("no bytes", &[]),
            ("an unknown kind", &[0]),
            ("a cut message", &heartbeat[..heartbeat.len() - 1]),
            ("a byte after the end", &tail),
            ("an unknown body", &body),
            ("an answer that is not 0 or 1", &answer),
            ("a grant with no signature", &unsigned),
            ("a proof byte that is not 0 or 1", &presence),
            ("a grant that is not 0 or 1", &grant),
            ("proof voters that do not rise", &proven([2, 1])),
            ("a proof voter twice", &proven([1, 1])),
            ("voters that do not rise", &falling),
            ("a voter twice", &twice),
        ];
        for (name, bytes) in cases {
            assert_eq!(Message::decode(bytes), None, "{name}");
        }
        assert!(Message::decode(&change_append([1, 2])).is_some());
        assert!(Message::decode(&proven([1, 2])).is_some());
        assert!(Message::decode(&granted).is_some());
    }

    #[test]
    fn decode_refuses_what_is_not_a_proposal_or_an_answer() {
        let home = [&[2, 1][..], &key(7), &key(8)].concat();
        let cases: [(&str, &[u8]); 7] = [
            ("a proposal with no change", &[2]),
            ("a change of an unknown kind", &[2, 9]),
            ("a cut change", &home[..home.len() - 1]),
            ("a byte after a change", &[&home[..], &[0]].concat()),
            ("a cut position", &[&[3][..], &le(2), &le(4)[..7]].concat()),
            (
                "a leader after \"not the leader\"",
                &[&[4][..], &key(7)].concat(),
            ),
            ("a cut leader", &[&[5][..], &key(7)[..15]].concat()),
        ];
        for (name, bytes) in cases {
            assert_eq!(Message::decode(bytes), None, "{name}");
        }
        assert!(Message::decode(&home).is_some());
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_grant() {
        raft(Body::PreVoteReply {
            answer: Answer::Granted(None),
        })
        .encode();
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_proof_entry() {
        Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body: Body::Vote { last: at(0, 0) },
            proof: Some(Proof {
                grant: Grant::PreVote,
                candidate: node(1),
                voters: [(node(1), None), (node(2), Some(signature(2)))].into(),
            }),
        })
        .encode();
    }

    proptest! {
        #[test]
        fn a_message_round_trips(message in a_message()) {
            prop_assert_eq!(Message::decode(&message.encode()), Some(message));
        }

        #[test]
        fn bytes_that_decode_are_the_byte_form(
            message in a_message(),
            at in any::<prop::sample::Index>(),
            byte in any::<u8>(),
        ) {
            let mut bytes = message.encode();
            let at = at.index(bytes.len());
            bytes[at] = byte;
            if let Some(found) = Message::decode(&bytes) {
                prop_assert_eq!(found.encode(), bytes);
            }
        }

        #[test]
        fn any_bytes_decode_without_a_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..256),
        ) {
            if let Some(found) = Message::decode(&bytes) {
                prop_assert_eq!(found.encode(), bytes);
            }
        }
    }
}
