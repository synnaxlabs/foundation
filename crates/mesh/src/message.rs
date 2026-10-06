//! The byte form of what two nodes of a region say on a mesh stream.
//!
//! A message has one byte form: [`Message::decode`] takes only what
//! [`Message::encode`] gives. The stream's header carries the format version.

use raft::{Body, Position, Term};
use types::node;

use crate::bytes::{put_key, put_position, take, take_key, take_position};
use crate::entry;
use crate::region::Change;

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
    /// Asks the leader to propose `change`.
    Propose {
        /// Pairs the answer with this message. The sender picks it.
        request: u64,
        /// The change.
        change: Change,
    },
    /// Answers a [`Message::Propose`] that the receiver proposed.
    Proposed {
        /// The `request` of the [`Message::Propose`].
        request: u64,
        /// The position of the entry. A new leader can replace it.
        at: Position,
    },
    /// Answers a [`Message::Propose`] that the receiver did not propose, because it
    /// does not lead.
    NotLeader {
        /// The `request` of the [`Message::Propose`].
        request: u64,
        /// The leader that the receiver knows.
        leader: Option<node::Key>,
    },
}

impl Message {
    /// The byte form of the message.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Raft(message) => {
                out.push(RAFT);
                put_key(message.from, &mut out);
                put_key(message.to, &mut out);
                out.extend(message.term.0.to_le_bytes());
                body(&message.body, &mut out);
            }
            Self::Propose { request, change } => {
                out.push(PROPOSE);
                out.extend(request.to_le_bytes());
                change.encode(&mut out);
            }
            Self::Proposed { request, at } => {
                out.push(PROPOSED);
                out.extend(request.to_le_bytes());
                put_position(*at, &mut out);
            }
            Self::NotLeader { request, leader } => {
                out.push(leader.map_or(NOT_LEADER, |_| NOT_LEADER_WITH_LEADER));
                out.extend(request.to_le_bytes());
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
            RAFT => Self::Raft(raft::Message {
                from: take_key(bytes)?,
                to: take_key(bytes)?,
                term: Term(u64::from_le_bytes(take(bytes)?)),
                body: take_body(bytes)?,
            }),
            PROPOSE => {
                let request = u64::from_le_bytes(take(bytes)?);
                let change = Change::decode(std::mem::take(bytes)).ok()?;
                Self::Propose { request, change }
            }
            PROPOSED => Self::Proposed {
                request: u64::from_le_bytes(take(bytes)?),
                at: take_position(bytes)?,
            },
            kind @ (NOT_LEADER | NOT_LEADER_WITH_LEADER) => Self::NotLeader {
                request: u64::from_le_bytes(take(bytes)?),
                leader: match kind {
                    NOT_LEADER => None,
                    _ => Some(take_key(bytes)?),
                },
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
        Body::PreVoteReply { granted } => {
            out.extend([PRE_VOTE_REPLY, u8::from(*granted)]);
        }
        Body::Vote { last } => {
            out.push(VOTE);
            put_position(*last, out);
        }
        Body::VoteReply { granted } => {
            out.extend([VOTE_REPLY, u8::from(*granted)]);
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
            granted: flag(bytes)?,
        },
        VOTE => Body::Vote {
            last: take_position(bytes)?,
        },
        VOTE_REPLY => Body::VoteReply {
            granted: flag(bytes)?,
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

fn flag(bytes: &mut &[u8]) -> Option<bool> {
    match u8::from_le_bytes(take(bytes)?) {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;
    use raft::{Data, Entry, Voters};
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

    fn an_entry() -> impl Strategy<Value = Entry> {
        let data = prop_oneof![
            Just(Data::Empty),
            prop::collection::vec(any::<u8>(), 0..48).prop_map(Data::Bytes),
            (keys(), keys()).prop_map(|(incoming, outgoing)| {
                Data::Voters(Voters { incoming, outgoing })
            }),
        ];
        (a_position(), data).prop_map(|(at, data)| Entry { at, data })
    }

    fn a_body() -> impl Strategy<Value = Body> {
        let entries = prop::collection::vec(an_entry(), 0..4);
        prop_oneof![
            a_position().prop_map(|last| Body::PreVote { last }),
            any::<bool>().prop_map(|granted| Body::PreVoteReply { granted }),
            a_position().prop_map(|last| Body::Vote { last }),
            any::<bool>().prop_map(|granted| Body::VoteReply { granted }),
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

    fn a_message() -> impl Strategy<Value = Message> {
        let raft = (any::<u128>(), any::<u128>(), any::<u64>(), a_body()).prop_map(
            |(from, to, term, body)| {
                Message::Raft(raft::Message {
                    from: node(from),
                    to: node(to),
                    term: Term(term),
                    body,
                })
            },
        );
        let propose = (any::<u64>(), any::<u128>(), any::<u128>()).prop_map(
            |(request, index, home)| Message::Propose {
                request,
                change: Change::Home {
                    index: channel::Key::from_u128(index),
                    home: node(home),
                },
            },
        );
        let proposed = (any::<u64>(), a_position())
            .prop_map(|(request, at)| Message::Proposed { request, at });
        let not_leader = (any::<u64>(), prop::option::of(any::<u128>())).prop_map(
            |(request, leader)| Message::NotLeader {
                request,
                leader: leader.map(node),
            },
        );
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

    fn raft(body: Body) -> Message {
        Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body,
        })
    }

    /// The bytes of [`raft`] before the body.
    fn head() -> Vec<u8> {
        [&[RAFT][..], &key(1), &key(2), &le(3)].concat()
    }

    #[test]
    fn each_body_has_a_fixed_byte_form() {
        let cases: [(Body, &[&[u8]]); 8] = [
            (Body::PreVote { last: at(2, 4) }, &[&[1], &le(2), &le(4)]),
            (Body::PreVoteReply { granted: true }, &[&[2, 1]]),
            (Body::Vote { last: at(2, 4) }, &[&[3], &le(2), &le(4)]),
            (Body::VoteReply { granted: false }, &[&[4, 0]]),
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
    fn a_proposal_and_its_answers_have_a_fixed_byte_form() {
        let propose = Message::Propose {
            request: 9,
            change: Change::Home {
                index: channel::Key::from_u128(7),
                home: node(8),
            },
        };
        let expected = [&[2][..], &le(9), &[1], &key(7), &key(8)].concat();
        assert_eq!(propose.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(propose));
        let proposed = Message::Proposed {
            request: 9,
            at: at(2, 4),
        };
        let expected = [&[3][..], &le(9), &le(2), &le(4)].concat();
        assert_eq!(proposed.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(proposed));
        let none = Message::NotLeader {
            request: 9,
            leader: None,
        };
        let expected = [&[4][..], &le(9)].concat();
        assert_eq!(none.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(none));
        let known = Message::NotLeader {
            request: 9,
            leader: Some(node(7)),
        };
        let expected = [&[5][..], &le(9), &key(7)].concat();
        assert_eq!(known.encode(), expected);
        assert_eq!(Message::decode(&expected), Some(known));
    }

    #[test]
    fn decode_refuses_what_is_not_a_message() {
        let heartbeat = Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body: Body::HeartbeatReply,
        })
        .encode();
        let granted = Message::Raft(raft::Message {
            from: node(1),
            to: node(2),
            term: Term(3),
            body: Body::VoteReply { granted: true },
        })
        .encode();
        let mut flag = granted.clone();
        *flag.last_mut().unwrap() = 2;
        let mut body = heartbeat.clone();
        *body.last_mut().unwrap() = 10;
        let mut tail = heartbeat.clone();
        tail.push(0);
        let voters = |keys: [u128; 2]| {
            Message::Raft(raft::Message {
                from: node(1),
                to: node(2),
                term: Term(3),
                body: Body::Append {
                    prev: at(0, 0),
                    entries: vec![Entry {
                        at: at(3, 1),
                        data: Data::Voters(Voters {
                            incoming: keys.map(node).into(),
                            outgoing: BTreeSet::new(),
                        }),
                    }],
                    commit: 0,
                },
            })
            .encode()
        };
        // Two keys that fall, and one key twice: the keys of each set are 16 bytes
        // before the count of the second set.
        let mut falling = voters([1, 2]);
        let len = falling.len();
        falling.swap(len - 40, len - 24);
        let mut twice = voters([1, 2]);
        twice.swap(len - 40, len - 24);
        twice[len - 40] = 2;
        twice[len - 24] = 2;
        let cases: [(&str, &[u8]); 9] = [
            ("no bytes", &[]),
            ("an unknown kind", &[0]),
            ("a cut message", &heartbeat[..heartbeat.len() - 1]),
            ("a byte after the end", &tail),
            ("an unknown body", &body),
            ("a flag that is not 0 or 1", &flag),
            (
                "a change that is not valid",
                &[2, 0, 0, 0, 0, 0, 0, 0, 0, 9],
            ),
            ("voters that do not rise", &falling),
            ("a voter twice", &twice),
        ];
        for (name, bytes) in cases {
            assert_eq!(Message::decode(bytes), None, "{name}");
        }
        assert!(Message::decode(&voters([1, 2])).is_some());
        assert!(Message::decode(&granted).is_some());
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
