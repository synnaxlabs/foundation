//! The byte form of what two nodes of a region say on a mesh stream.
//!
//! A message has one byte form: [`Message::decode`] takes only what
//! [`Message::encode`] gives. The stream's header carries the format version.

use raft::{Answer, Body, Link, Position, Term};
use types::node;

use crate::bytes::{
    change_len, proof_len, put_change, put_count, put_key, put_optional_proof,
    put_position, put_signature, take, take_bool, take_change, take_count, take_key,
    take_position, take_proof, take_signature,
};
use crate::change::Change;
use crate::entry;
use crate::refusal::Refusal;
use crate::region::Request;

const RAFT: u8 = 1;
const PROPOSE: u8 = 2;
const PROPOSED: u8 = 3;
const NOT_LEADER: u8 = 4;
const NOT_LEADER_WITH_LEADER: u8 = 5;
const JOIN: u8 = 6;
const JOINED: u8 = 7;
const UNJOINED: u8 = 8;
const FOUNDING: u8 = 9;

const PRE_VOTE: u8 = 1;
const PRE_VOTE_REPLY: u8 = 2;
const VOTE: u8 = 3;
const VOTE_REPLY: u8 = 4;
const HEARTBEAT: u8 = 5;
const HEARTBEAT_REPLY: u8 = 6;
const APPEND: u8 = 7;
const APPEND_REPLY: u8 = 8;
const APPEND_REJECT: u8 = 9;

/// The bytes of an `Append` with no proof and no link, of one entry of
/// [`entry::BYTES_MAX`] bytes. Each member takes a message of this size.
pub(crate) const BYTES_MIN: usize = APPEND_HEAD + entry::BYTES_MAX;

// The bytes of an `Append` with no entry, no proof, and no link.
const APPEND_HEAD: usize = 75;

/// The most voters of one set of a configuration. A joint change of two such sets,
/// with a proof of each of their grants, fits a message of [`BYTES_MIN`] bytes.
pub(crate) const VOTERS_MAX: usize = 127;

// The grants of the two sets of a joint configuration.
const GRANTS_MAX: usize = 2 * VOTERS_MAX;

/// The most bytes of the byte form of a founding that a join answer holds: 64 MiB.
/// No later build makes it smaller, or a region that one build founds has no join.
pub(crate) const FOUNDING_BYTES_MAX: usize = 64 << 20;

/// The bytes of a [`Message::Founding`] before its part.
pub(crate) const FOUNDING_HEAD: usize = 1;

/// Writes the byte form of a [`Message::Founding`] of `part` to `out`, which is
/// [`FOUNDING_HEAD`] bytes longer than `part`.
///
/// # Panics
///
/// When `out` has another length.
pub(crate) fn put_founding(part: &[u8], out: &mut [u8]) {
    let (head, rest) = out.split_at_mut(FOUNDING_HEAD);
    head.copy_from_slice(&[FOUNDING]);
    rest.copy_from_slice(part);
}

/// Whether the byte form of `message` is at most [`BYTES_MIN`] bytes, for
/// `raft::Config::fits`.
pub(crate) fn fits(message: &raft::Message) -> bool {
    len(message) <= BYTES_MIN
}

// The bytes of the byte form of `Message::Raft(message)`, with 64 for each signature,
// also one not yet made.
fn len(message: &raft::Message) -> usize {
    let proof = message.proof.as_ref().map_or(0, proof_len);
    let links = message.chain.iter();
    let chain = links.map(|link| change_len(&link.change).saturating_add(16));
    let answer = |answer: &Answer| match answer {
        Answer::Refused => 1,
        Answer::Granted(_) => 1 + 64,
    };
    let body = match &message.body {
        Body::PreVote { .. } | Body::Vote { .. } => 16,
        Body::PreVoteReply { answer: a } | Body::VoteReply { answer: a } => answer(a),
        Body::Heartbeat { .. }
        | Body::AppendReply { .. }
        | Body::AppendReject { .. } => 8,
        Body::HeartbeatReply => 0,
        Body::Append { entries, .. } => entry::len(entries).saturating_add(16 + 8),
    };
    let head = 1 + 16 + 16 + 8 + 1 + 8 + 1;
    chain
        .fold(head, usize::saturating_add)
        .saturating_add(proof)
        .saturating_add(body)
}

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
    /// Asks a voter to admit the sender by a ticket. It is the one message of its
    /// stream, and the answer comes on the reply half.
    Join {
        /// The request.
        request: Box<Request>,
    },
    /// Answers a [`Message::Join`] that the receiver admits. The next messages of the
    /// reply half are each a [`Message::Founding`], and their parts make the byte
    /// form of the founding of the region.
    Joined {
        /// The length of the byte form of the founding, at most
        /// [`FOUNDING_BYTES_MAX`].
        bytes: usize,
    },
    /// Answers a [`Message::Join`] that the receiver does not admit.
    Unjoined {
        /// Why.
        refusal: Refusal,
    },
    /// The next part of the byte form of the founding, after a [`Message::Joined`].
    Founding {
        /// The bytes of the part.
        part: Vec<u8>,
    },
}

impl Message {
    /// The byte form of the message.
    ///
    /// # Panics
    ///
    /// When a claim has no signature: the caller signs each claim first.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Raft(message) => {
                out.push(RAFT);
                put_key(message.from, &mut out);
                put_key(message.to, &mut out);
                out.extend(message.term.0.to_le_bytes());
                put_optional_proof(message.proof.as_ref(), &mut out);
                put_chain(&message.chain, &mut out);
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
            Self::Join { request } => {
                out.push(JOIN);
                request.encode(&mut out);
            }
            Self::Joined { bytes } => {
                out.push(JOINED);
                put_count(*bytes, &mut out);
            }
            Self::Unjoined { refusal } => {
                out.push(UNJOINED);
                refusal.encode(&mut out);
            }
            Self::Founding { part } => {
                let bytes = part.len().checked_add(FOUNDING_HEAD);
                out.resize(bytes.expect("invariant: a part fits in memory"), 0);
                put_founding(part, &mut out);
            }
        }
        out
    }

    /// The message that `bytes` is the byte form of, or `None` when it is the byte
    /// form of no message, of an `Append` with an entry of more than
    /// [`entry::BYTES_MAX`] bytes, of a proof of more grants than two sets of
    /// [`VOTERS_MAX`] voters, or of a `Joined` of more than [`FOUNDING_BYTES_MAX`]
    /// bytes.
    pub(crate) fn decode(mut bytes: &[u8]) -> Option<Self> {
        let bytes = &mut bytes;
        let message = match u8::from_le_bytes(take(bytes)?) {
            RAFT => {
                let from = take_key(bytes)?;
                let to = take_key(bytes)?;
                let term = Term(u64::from_le_bytes(take(bytes)?));
                let proof = if take_bool(bytes)? {
                    let proof = take_proof(bytes)?;
                    if proof.voters.len() > GRANTS_MAX {
                        return None;
                    }
                    Some(proof)
                } else {
                    None
                };
                let chain = take_chain(bytes)?;
                Self::Raft(raft::Message {
                    from,
                    to,
                    term,
                    body: take_body(bytes)?,
                    proof,
                    chain,
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
            JOIN => Self::Join {
                request: Box::new(Request::decode(bytes)?),
            },
            JOINED => Self::Joined {
                bytes: usize::try_from(take_count(bytes)?)
                    .ok()
                    .filter(|&bytes| bytes <= FOUNDING_BYTES_MAX)?,
            },
            UNJOINED => Self::Unjoined {
                refusal: Refusal::decode(bytes)?,
            },
            FOUNDING => Self::Founding {
                part: std::mem::take(bytes).to_vec(),
            },
            _ => return None,
        };
        bytes.is_empty().then_some(message)
    }
}

// A chain is a count of links as 8 little-endian bytes, then each link's position
// and change.
fn put_chain(chain: &[Link], out: &mut Vec<u8>) {
    put_count(chain.len(), out);
    for link in chain {
        put_position(link.at, out);
        put_change(&link.change, out);
    }
}

fn take_chain(bytes: &mut &[u8]) -> Option<Vec<Link>> {
    let count = take_count(bytes)?;
    let mut chain = Vec::new();
    for _ in 0..count {
        let at = take_position(bytes)?;
        let change = take_change(bytes)?;
        chain.push(Link { at, change });
    }
    Some(chain)
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
            entry::encode(entries, out);
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
            let entries = entry::decode(std::mem::take(bytes))?;
            // `raft` must fit each entry in an `Append` alone.
            let alone = |one| entry::len(std::slice::from_ref(one)) <= entry::BYTES_MAX;
            if !entries.iter().all(alone) {
                return None;
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

    /// The bytes of a link whose change has three incoming voters, three outgoing
    /// voters, and three votes: the position, two key sets, the votes, and the
    /// signature.
    const LINK: usize = 16 + (8 + 3 * 16) * 2 + (1 + 16 + 8 + 3 * (16 + 64)) + 64;

    use super::*;
    use crate::change;
    use crate::common::{at, key, member, name, public, ticket, unchecked};

    fn a_position() -> impl Strategy<Value = Position> {
        (any::<u64>(), any::<u64>()).prop_map(|(term, index)| at(term, index))
    }

    fn keys() -> impl Strategy<Value = BTreeSet<node::Key>> {
        prop::collection::btree_set(any::<u128>().prop_map(node::Key::from_u128), 0..4)
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

    fn a_change() -> impl Strategy<Value = raft::Change> {
        (keys(), keys(), a_proof(), a_signature()).prop_map(
            |(incoming, outgoing, votes, signature)| raft::Change {
                voters: Voters { incoming, outgoing },
                votes,
                signature: Some(signature),
            },
        )
    }

    fn an_entry() -> impl Strategy<Value = Entry> {
        let data = prop_oneof![
            Just(Data::Empty),
            prop::collection::vec(any::<u8>(), 0..48).prop_map(Data::Bytes),
            a_change().prop_map(Data::Voters),
        ];
        (a_position(), data).prop_map(|(at, data)| Entry { at, data })
    }

    fn a_link() -> impl Strategy<Value = Link> {
        (a_position(), a_change()).prop_map(|(at, change)| Link { at, change })
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
            any::<u128>().prop_map(node::Key::from_u128),
            a_signature().prop_map(Some),
            0..4,
        );
        (grant, any::<u128>(), voters).prop_map(|(grant, candidate, voters)| Proof {
            grant,
            candidate: node::Key::from_u128(candidate),
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
            prop::collection::vec(a_link(), 0..3),
        );
        let raft = fields.prop_map(|(from, to, term, body, proof, chain)| {
            Message::Raft(raft::Message {
                from: node::Key::from_u128(from),
                to: node::Key::from_u128(to),
                term: Term(term),
                body,
                proof,
                chain,
            })
        });
        let propose =
            (any::<u128>(), any::<u128>()).prop_map(|(index, home)| Message::Propose {
                change: Change::Home {
                    index: channel::Key::from_u128(index),
                    home: node::Key::from_u128(home),
                },
            });
        let proposed = a_position().prop_map(|at| Message::Proposed { at });
        let not_leader =
            prop::option::of(any::<u128>()).prop_map(|leader| Message::NotLeader {
                leader: leader.map(node::Key::from_u128),
            });
        let join = (1..=5_u8, 0..=3_u8).prop_map(|(id, count)| Message::Join {
            request: Box::new(request(id, count)),
        });
        let joined =
            (0..=FOUNDING_BYTES_MAX).prop_map(|bytes| Message::Joined { bytes });
        let founding = prop::collection::vec(any::<u8>(), 0..=16)
            .prop_map(|part| Message::Founding { part });
        prop_oneof![
            4 => raft,
            1 => propose,
            1 => proposed,
            1 => not_leader,
            1 => join,
            1 => joined,
            1 => crate::refusal::tests::refusals()
                .prop_map(|refusal| Message::Unjoined { refusal }),
            1 => founding,
        ]
    }

    // The join of node `id` by ticket 1, with `count` status names.
    fn request(id: u8, count: u8) -> Request {
        let card = member(id).card;
        Request {
            ticket: public(1),
            card: unchecked(&card),
            admission: ticket(1).admission(&card),
            status: (0..count).map(|i| name(&format!("s{i}"))).collect(),
        }
    }

    fn le(number: u64) -> [u8; 8] {
        number.to_le_bytes()
    }

    fn le_key(low: u8) -> Vec<u8> {
        let mut key = vec![low];
        key.extend([0; 15]);
        key
    }

    fn signature(byte: u8) -> Signature {
        Signature([byte; 64])
    }

    // An append of one change to the incoming voters `keys`, with no outgoing voter.
    fn change_append(keys: [u8; 2]) -> Vec<u8> {
        raft(Body::Append {
            prev: at(0, 0),
            entries: vec![Entry {
                at: at(3, 1),
                data: Data::Voters(raft::Change {
                    voters: Voters {
                        incoming: keys.map(key).into(),
                        outgoing: BTreeSet::new(),
                    },
                    votes: Proof {
                        grant: Grant::Vote,
                        candidate: key(1),
                        voters: [(key(1), Some(signature(1)))].into(),
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
            from: key(1),
            to: key(2),
            term: Term(3),
            body,
            proof: None,
            chain: Vec::new(),
        })
    }

    /// The bytes of [`raft`] before the body: no proof, then an empty chain.
    fn head() -> Vec<u8> {
        [&[RAFT][..], &le_key(1), &le_key(2), &le(3), &[0], &le(0)].concat()
    }

    /// The length of [`head`].
    const HEAD: usize = 1 + 16 + 16 + 8 + 1 + 8;

    /// Where the proof's presence byte is in [`head`].
    const PROOF_AT: usize = HEAD - 9;

    /// The change of leader 1 to the incoming voters 1 and 2 from the outgoing
    /// voter 2, with the vote of voter 1 and `signature`.
    fn change(signature: Option<Signature>) -> raft::Change {
        raft::Change {
            voters: Voters {
                incoming: [key(1), key(2)].into(),
                outgoing: [key(2)].into(),
            },
            votes: Proof {
                grant: Grant::Vote,
                candidate: key(1),
                voters: [(key(1), Some(self::signature(5)))].into(),
            },
            signature,
        }
    }

    // `proof` with no signature made.
    fn unsigned_proof(proof: &mut Proof) {
        for signature in proof.voters.values_mut() {
            *signature = None;
        }
    }

    fn unsigned_change(change: &mut raft::Change) {
        unsigned_proof(&mut change.votes);
        change.signature = None;
    }

    // `message` with no signature made, as `raft` gives it before `Ready::sign`.
    fn unsigned(mut message: raft::Message) -> raft::Message {
        message.proof.as_mut().map(unsigned_proof);
        for link in &mut message.chain {
            unsigned_change(&mut link.change);
        }
        match &mut message.body {
            Body::PreVoteReply { answer } | Body::VoteReply { answer } => {
                if let Answer::Granted(signature) = answer {
                    *signature = None;
                }
            }
            Body::Append { entries, .. } => {
                for entry in entries {
                    if let Data::Voters(change) = &mut entry.data {
                        unsigned_change(change);
                    }
                }
            }
            Body::PreVote { .. }
            | Body::Vote { .. }
            | Body::Heartbeat { .. }
            | Body::HeartbeatReply
            | Body::AppendReply { .. }
            | Body::AppendReject { .. } => {}
        }
        message
    }

    // The keys `from..to`.
    fn keys_of(from: u128, to: u128) -> BTreeSet<node::Key> {
        (from..to).map(node::Key::from_u128).collect()
    }

    // The votes of 254 voters for voter 0, none yet signed.
    fn votes_of_254() -> Proof {
        Proof {
            grant: Grant::Vote,
            candidate: key(0),
            voters: keys_of(0, 254).into_iter().map(|key| (key, None)).collect(),
        }
    }

    /// A heartbeat of [`raft`] with `chain`.
    fn chained(chain: Vec<Link>) -> Message {
        Message::Raft(raft::Message {
            chain,
            ..match raft(Body::Heartbeat { commit: 6 }) {
                Message::Raft(message) => message,
                _ => unreachable!(),
            }
        })
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
        assert_eq!(head().len(), HEAD);
        let message = Message::Raft(raft::Message {
            from: key(1),
            to: key(2),
            term: Term(3),
            body: Body::Heartbeat { commit: 6 },
            proof: Some(Proof {
                grant: Grant::Vote,
                candidate: key(1),
                voters: [(key(1), Some(signature(5))), (key(4), Some(signature(6)))]
                    .into(),
            }),
            chain: Vec::new(),
        });
        let voters = [&le(2)[..], &le_key(1), &[5; 64], &le_key(4), &[6; 64]].concat();
        let proof = [&[1, 1][..], &le_key(1), &voters].concat();
        let head = &head()[..PROOF_AT];
        let expected = [head, &proof, &le(0), &[5], &le(6)].concat();
        assert_eq!(message.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(message));
    }

    #[test]
    fn a_chain_has_a_fixed_byte_form() {
        let link = Link {
            at: at(2, 4),
            change: change(Some(signature(6))),
        };
        let message = chained(vec![link]);
        let position = [le(2), le(4)].concat();
        let incoming = [&le(2)[..], &le_key(1), &le_key(2)].concat();
        let outgoing = [&le(1)[..], &le_key(2)].concat();
        let votes = [&[1][..], &le_key(1), &le(1), &le_key(1), &[5; 64]].concat();
        let link = [position, incoming, outgoing, votes, vec![6; 64]].concat();
        let head = &head()[..head().len() - 8];
        let expected = [head, &le(1), &link, &[5], &le(6)].concat();
        assert_eq!(message.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(message));
    }

    #[test]
    fn a_link_of_three_voters_is_457_bytes() {
        assert_eq!(LINK, 457);
        let bare = chained(Vec::new()).encode().len();
        for count in [2, 20] {
            let chain = (1..=count)
                .map(|index| Link {
                    at: at(2, index),
                    change: raft::Change {
                        voters: Voters {
                            incoming: [1, 2, 3].map(key).into(),
                            outgoing: [1, 2, 4].map(key).into(),
                        },
                        votes: Proof {
                            grant: Grant::Vote,
                            candidate: key(1),
                            voters: [1_u8, 2, 3]
                                .map(|id| (key(id), Some(signature(id))))
                                .into(),
                        },
                        signature: Some(signature(6)),
                    },
                })
                .collect();
            let encoded = chained(chain).encode().len();
            assert_eq!(encoded - bare, LINK * usize::try_from(count).unwrap());
        }
    }

    #[test]
    fn a_proposal_and_its_answers_have_a_fixed_byte_form() {
        let propose = Message::Propose {
            change: Change::Home {
                index: channel::Key::from_u128(7),
                home: key(8),
            },
        };
        let known = Message::NotLeader {
            leader: Some(key(7)),
        };
        let cases: [(Message, &[&[u8]]); 4] = [
            (propose, &[&[2, 1], &le_key(7), &le_key(8)]),
            (Message::Proposed { at: at(2, 4) }, &[&[3], &le(2), &le(4)]),
            (Message::NotLeader { leader: None }, &[&[4]]),
            (known, &[&[5], &le_key(7)]),
        ];
        for (message, expected) in cases {
            let expected = expected.concat();
            assert_eq!(message.encode(), expected, "{message:?}");
            assert_eq!(Message::decode(&expected), Some(message));
        }
    }

    #[test]
    fn a_join_is_its_kind_then_the_request() {
        let request = request(2, 2);
        let mut expected = vec![6];
        request.encode(&mut expected);
        let join = Message::Join {
            request: Box::new(request),
        };
        assert_eq!(join.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(join));
    }

    // A joining node refuses a founding over the bound before it reads a part.
    #[test]
    fn a_joined_is_its_kind_then_the_length_up_to_the_most_of_a_founding() {
        let most = u64::try_from(FOUNDING_BYTES_MAX).unwrap();
        let joined = Message::Joined {
            bytes: FOUNDING_BYTES_MAX,
        };
        let expected = [&[7][..], &most.to_le_bytes()].concat();
        assert_eq!(joined.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(joined));
        let over = [&[7][..], &(most + 1).to_le_bytes()].concat();
        assert_eq!(Message::decode(&over), None);
        let part = Message::Founding { part: vec![4, 5] };
        assert_eq!(part.encode(), [9, 4, 5]);
        assert_eq!(Message::decode(&[9, 4, 5]), Some(part));
        let mut out = [0; 3];
        put_founding(&[4, 5], &mut out);
        assert_eq!(out, [9, 4, 5]);
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
        let mut presence = heartbeat.clone();
        presence[PROOF_AT] = 2;
        let grant = [
            &head()[..PROOF_AT],
            &[1, 2],
            &le_key(1),
            &le(0),
            &le(0),
            &[6],
        ]
        .concat();
        let proven = |voters: [u8; 2]| {
            let voters = voters.map(|voter| [le_key(voter), vec![voter; 64]].concat());
            let proof = [&[1, 1][..], &le_key(1), &le(2), &voters.concat()].concat();
            [&head()[..PROOF_AT], &proof, &le(0), &[6]].concat()
        };
        let link = Link {
            at: at(2, 4),
            change: change(Some(signature(6))),
        };
        let chained = chained(vec![link]).encode();
        let cut_link = [&chained[..head().len() + 16], &[5], &le(6)].concat();
        let mut counted = heartbeat.clone();
        counted[head().len() - 8] = 1;
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
        let cases: [(&str, &[u8]); 15] = [
            ("a chain cut in a link", &cut_link),
            ("a chain count with no link", &counted),
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
        assert!(Message::decode(&chained).is_some());
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_link() {
        chained(vec![Link {
            at: at(2, 4),
            change: change(None),
        }])
        .encode();
    }

    #[test]
    fn decode_refuses_what_is_not_a_proposal_or_an_answer() {
        let home = [&[2, 1][..], &le_key(7), &le_key(8)].concat();
        let cases: [(&str, &[u8]); 7] = [
            ("a proposal with no change", &[2]),
            ("a change of an unknown kind", &[2, 9]),
            ("a cut change", &home[..home.len() - 1]),
            ("a byte after a change", &[&home[..], &[0]].concat()),
            ("a cut position", &[&[3][..], &le(2), &le(4)[..7]].concat()),
            (
                "a leader after \"not the leader\"",
                &[&[4][..], &le_key(7)].concat(),
            ),
            ("a cut leader", &[&[5][..], &le_key(7)[..15]].concat()),
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
            from: key(1),
            to: key(2),
            term: Term(3),
            body: Body::Vote { last: at(0, 0) },
            proof: Some(Proof {
                grant: Grant::PreVote,
                candidate: key(1),
                voters: [(key(1), None), (key(2), Some(signature(2)))].into(),
            }),
            chain: Vec::new(),
        })
        .encode();
    }

    #[test]
    fn decode_refuses_an_append_of_a_record_over_bytes_max_bytes() {
        let append = |bytes| {
            raft(Body::Append {
                prev: at(0, 0),
                entries: vec![Entry {
                    at: at(1, 1),
                    data: Data::Bytes(vec![0; bytes]),
                }],
                commit: 0,
            })
        };
        let most = append(change::BYTES_MAX);
        assert_eq!(Message::decode(&most.encode()), Some(most));
        assert_eq!(
            Message::decode(&append(change::BYTES_MAX + 1).encode()),
            None
        );
    }

    #[test]
    fn decode_refuses_a_proof_of_more_grants_than_two_sets_of_voters_max() {
        let proven = |grants| {
            let Message::Raft(message) = raft(Body::HeartbeatReply) else {
                unreachable!()
            };
            let mut proof = votes_of_254();
            proof.voters = keys_of(0, grants)
                .into_iter()
                .map(|key| (key, Some(Signature([7; 64]))))
                .collect();
            Message::Raft(raft::Message {
                proof: Some(proof),
                ..message
            })
        };
        let most = u128::try_from(2 * VOTERS_MAX).unwrap();
        assert_eq!(most, 254);
        assert_eq!(Message::decode(&proven(most).encode()), Some(proven(most)));
        assert_eq!(Message::decode(&proven(most + 1).encode()), None);
    }

    #[test]
    fn decode_refuses_an_append_whose_last_entry_is_over_bytes_max_bytes() {
        let append = |bytes| {
            let entry = |index, bytes| Entry {
                at: at(1, index),
                data: Data::Bytes(vec![0; bytes]),
            };
            raft(Body::Append {
                prev: at(0, 0),
                entries: vec![entry(1, 1), entry(2, bytes)],
                commit: 0,
            })
        };
        let most = append(change::BYTES_MAX);
        assert_eq!(Message::decode(&most.encode()), Some(most));
        assert_eq!(
            Message::decode(&append(change::BYTES_MAX + 1).encode()),
            None
        );
    }

    #[test]
    fn decode_refuses_an_append_of_a_change_over_bytes_max_and_8_bytes() {
        let append = |voters: u128| {
            let change = raft::Change {
                voters: Voters {
                    incoming: (1..=voters).map(node::Key::from_u128).collect(),
                    outgoing: BTreeSet::new(),
                },
                votes: Proof {
                    grant: Grant::Vote,
                    candidate: key(1),
                    voters: [(key(1), Some(signature(1)))].into(),
                },
                signature: Some(signature(2)),
            };
            raft(Body::Append {
                prev: at(0, 0),
                entries: vec![Entry {
                    at: at(1, 1),
                    data: Data::Voters(change),
                }],
                commit: 0,
            })
        };
        // The bytes of the change: the message less its head, the entry's position, and
        // its kind.
        let bytes = |voters| append(voters).encode().len() - (APPEND_HEAD + 16 + 1);
        let most = 1 + u128::try_from(change::BYTES_MAX - bytes(1)).unwrap() / 16;
        assert_eq!(bytes(most), change::BYTES_MAX - 6);
        assert_eq!(bytes(most + 1), change::BYTES_MAX + 10);
        assert_eq!(Message::decode(&append(most).encode()), Some(append(most)));
        assert_eq!(Message::decode(&append(most + 1).encode()), None);
    }

    #[test]
    fn an_append_of_one_record_of_bytes_max_bytes_is_bytes_min_bytes() {
        let record = Entry {
            at: at(1, 1),
            data: Data::Bytes(vec![0; change::BYTES_MAX]),
        };
        let Message::Raft(mut message) = raft(Body::Append {
            prev: at(0, 0),
            entries: vec![record],
            commit: 0,
        }) else {
            unreachable!()
        };
        assert_eq!(Message::Raft(message.clone()).encode().len(), BYTES_MIN);
        assert!(fits(&message));
        let Body::Append { entries, .. } = &mut message.body else {
            unreachable!()
        };
        let Data::Bytes(bytes) = &mut entries[0].data else {
            unreachable!()
        };
        bytes.push(0);
        assert!(!fits(&message));
    }

    #[test]
    fn a_joint_change_of_127_and_127_voters_and_its_largest_proof_fit() {
        let most = u128::try_from(VOTERS_MAX).unwrap();
        let change = raft::Change {
            voters: Voters {
                incoming: keys_of(0, most),
                outgoing: keys_of(most, 2 * most),
            },
            votes: votes_of_254(),
            signature: None,
        };
        let Message::Raft(append) = raft(Body::Append {
            prev: at(0, 0),
            entries: vec![Entry {
                at: at(1, 1),
                data: Data::Voters(change),
            }],
            commit: 0,
        }) else {
            unreachable!()
        };
        let reply = raft::Message {
            proof: Some(votes_of_254()),
            body: Body::VoteReply {
                answer: Answer::Granted(None),
            },
            ..append.clone()
        };
        assert_eq!([len(&append), len(&reply)], [24_581, 20_461]);
        assert!(fits(&append) && fits(&reply));
    }

    proptest! {
        #[test]
        fn len_is_the_length_of_the_byte_form_also_with_no_signature_made(
            message in a_message(),
        ) {
            if let Message::Raft(raft) = &message {
                let length = message.encode().len();
                prop_assert_eq!(len(raft), length);
                prop_assert_eq!(len(&unsigned(raft.clone())), length);
            }
        }

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
