//! Entry points for the fuzz targets. Needs the `sim` feature.

use crate::change::Change;
use crate::entry;
use crate::message::Message;

/// The bytes that the change record in `bytes` encodes to, or `None` when `bytes` is
/// not a change record.
#[must_use]
pub fn round_trip_change(bytes: &[u8]) -> Option<Vec<u8>> {
    let change = Change::decode(bytes).ok()?;
    let mut out = Vec::new();
    change.encode(&mut out);
    Some(out)
}

/// The bytes that the mesh message in `bytes` encodes to, or `None` when `bytes` is
/// not a mesh message.
#[must_use]
pub fn round_trip_message(bytes: &[u8]) -> Option<Vec<u8>> {
    Message::decode(bytes).map(|message| message.encode())
}

/// The bytes that the `raft` entries in `bytes` encode to, or `None` when `bytes` is
/// not entries one after another, as an append and the log hold them. Empty bytes
/// hold no entries and give empty bytes.
#[must_use]
pub fn round_trip_entries(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    entry::encode(&entry::decode(bytes)?, &mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ops::Range;

    use raft::{
        Answer, Body, Data, Entry, Grant, Link, Position, Proof, Signature, Term,
        Voters,
    };

    use super::*;
    use crate::bytes::{put_change, put_position};
    use crate::common::key;

    #[test]
    fn a_change_gives_its_bytes_and_other_bytes_give_none() {
        let mut home = vec![1, 0x02, 0x01];
        home.extend([0; 14]);
        home.extend([0x0b, 0x0a]);
        home.extend([0; 14]);
        assert_eq!(round_trip_change(&home), Some(home.clone()));
        assert_eq!(round_trip_change(&home[..32]), None);
        assert_eq!(round_trip_change(&[]), None);
    }

    // The inputs of a fuzz target, by name.
    macro_rules! inputs {
        ($target:literal: $($name:literal),+ $(,)?) => {
            BTreeMap::from([$((
                $name,
                &include_bytes!(concat!(
                    "../../../oracles/fuzz/", $target, "/", $name
                ))[..],
            )),+])
        };
    }

    fn count_files(target: &str) -> usize {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../oracles/fuzz/");
        std::fs::read_dir(format!("{root}{target}"))
            .unwrap()
            .count()
    }

    fn at(term: u64, index: u64) -> Position {
        Position {
            term: Term(term),
            index,
        }
    }

    fn proof() -> Proof {
        Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [
                (key(1), Some(Signature([7; 64]))),
                (key(2), Some(Signature([8; 64]))),
            ]
            .into(),
        }
    }

    fn change() -> raft::Change {
        raft::Change {
            voters: Voters {
                incoming: [key(1), key(2)].into(),
                outgoing: [key(1)].into(),
            },
            votes: proof(),
            signature: Some(Signature([9; 64])),
        }
    }

    /// An entry of each data kind: empty, bytes, and voters.
    fn entries() -> [Entry; 3] {
        [
            Entry {
                at: at(3, 4),
                data: Data::Empty,
            },
            Entry {
                at: at(3, 5),
                data: Data::Bytes(b"abc".to_vec()),
            },
            Entry {
                at: at(3, 6),
                data: Data::Voters(change()),
            },
        ]
    }

    // The bytes of each field of the change in the input `voters`, after its position
    // and tag.
    const FIELDS: [(&str, Range<usize>); 11] = [
        ("incoming_count", 17..25),
        ("incoming_key", 25..57),
        ("outgoing_count", 57..65),
        ("outgoing_key", 65..81),
        ("grant", 81..82),
        ("candidate", 82..98),
        ("vote_count", 98..106),
        ("vote_key", 106..122),
        ("vote_signature", 122..186),
        ("second_vote", 186..266),
        ("signature", 266..330),
    ];

    #[test]
    fn each_entries_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_entries":
            "no_entries",
            "empty",
            "bytes",
            "voters",
            "three",
            "bytes_length_past_end",
            "tag_3",
            "voters_cut_in_incoming_count",
            "voters_cut_in_incoming_key",
            "voters_cut_in_outgoing_count",
            "voters_cut_in_outgoing_key",
            "voters_cut_in_grant",
            "voters_cut_in_candidate",
            "voters_cut_in_vote_count",
            "voters_cut_in_vote_key",
            "voters_cut_in_vote_signature",
            "voters_cut_in_second_vote",
            "voters_cut_in_signature",
            "voters_incoming_falling",
        );
        assert_eq!(count_files("mesh_entries"), inputs.len());
        let [empty, bytes, voters] = entries();
        for (name, want) in [
            ("no_entries", vec![]),
            ("empty", vec![empty.clone()]),
            ("bytes", vec![bytes.clone()]),
            ("voters", vec![voters.clone()]),
            ("three", vec![empty, bytes, voters]),
        ] {
            assert_eq!(entry::decode(inputs[name]), Some(want), "{name}");
            let round_trip = round_trip_entries(inputs[name]);
            assert_eq!(round_trip.as_deref(), Some(inputs[name]), "{name}");
        }

        let mut refused = Vec::new();
        let bytes = inputs["bytes"];
        refused.push(("bytes_length_past_end", bytes[..bytes.len() - 1].to_vec()));
        let mut tag = inputs["empty"].to_vec();
        tag[16] = 3;
        refused.push(("tag_3", tag));
        let voters = inputs["voters"];
        assert_eq!(FIELDS[FIELDS.len() - 1].1.end, voters.len());
        for (field, range) in FIELDS {
            let name = format!("voters_cut_in_{field}");
            let cut = inputs[name.as_str()];
            assert!(range.start <= cut.len() && cut.len() < range.end, "{name}");
            assert_eq!(cut, &voters[..cut.len()], "{name}");
            assert_eq!(round_trip_entries(cut), None, "{name}");
        }
        let mut falling = voters.to_vec();
        falling[25..41].copy_from_slice(&key(3).as_u128().to_le_bytes());
        refused.push(("voters_incoming_falling", falling));
        for (name, want) in refused {
            assert_eq!(inputs[name], want, "{name}");
            assert_eq!(round_trip_entries(inputs[name]), None, "{name}");
        }
    }

    fn message(body: Body, proof: Option<Proof>, chain: Vec<Link>) -> Message {
        Message::Raft(raft::Message {
            from: key(1),
            to: key(2),
            term: Term(5),
            body,
            proof,
            chain,
        })
    }

    fn link() -> Link {
        Link {
            at: at(2, 3),
            change: change(),
        }
    }

    /// The message that each valid input of `mesh_message` holds.
    fn messages() -> [(&'static str, Message); 15] {
        let raft = |body| message(body, None, Vec::new());
        let heartbeat = || Body::Heartbeat { commit: 4 };
        let home = include_bytes!("../../../oracles/fuzz/mesh_change/home");
        [
            ("raft_pre_vote", raft(Body::PreVote { last: at(3, 6) })),
            (
                "raft_pre_vote_reply",
                raft(Body::PreVoteReply {
                    answer: Answer::Refused,
                }),
            ),
            ("raft_vote", raft(Body::Vote { last: at(3, 6) })),
            (
                "raft_vote_reply",
                raft(Body::VoteReply {
                    answer: Answer::Granted(Some(Signature([6; 64]))),
                }),
            ),
            ("raft_heartbeat", raft(heartbeat())),
            ("raft_heartbeat_reply", raft(Body::HeartbeatReply)),
            (
                "raft_append",
                raft(Body::Append {
                    prev: at(2, 3),
                    entries: entries().into(),
                    commit: 4,
                }),
            ),
            ("raft_append_reply", raft(Body::AppendReply { last: 6 })),
            ("raft_append_reject", raft(Body::AppendReject { hint: 3 })),
            (
                "raft_proof",
                message(heartbeat(), Some(proof()), Vec::new()),
            ),
            (
                "raft_chain",
                message(heartbeat(), Some(proof()), vec![link()]),
            ),
            (
                "propose",
                Message::Propose {
                    change: Change::decode(home).unwrap(),
                },
            ),
            ("proposed", Message::Proposed { at: at(3, 6) }),
            ("not_leader", Message::NotLeader { leader: None }),
            (
                "not_leader_with_leader",
                Message::NotLeader {
                    leader: Some(key(3)),
                },
            ),
        ]
    }

    #[test]
    fn each_message_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_message":
            "raft_pre_vote",
            "raft_pre_vote_reply",
            "raft_vote",
            "raft_vote_reply",
            "raft_heartbeat",
            "raft_heartbeat_reply",
            "raft_append",
            "raft_append_reply",
            "raft_append_reject",
            "raft_proof",
            "raft_chain",
            "propose",
            "proposed",
            "not_leader",
            "not_leader_with_leader",
            "none",
            "kind_6",
            "raft_body_10",
            "raft_heartbeat_trailing",
            "raft_chain_cut_in_link",
            "raft_chain_count_no_link",
        );
        assert_eq!(count_files("mesh_message"), inputs.len());
        for (name, want) in messages() {
            assert_eq!(Message::decode(inputs[name]), Some(want), "{name}");
            let round_trip = round_trip_message(inputs[name]);
            assert_eq!(round_trip.as_deref(), Some(inputs[name]), "{name}");
        }

        let mut refused = vec![("none", vec![]), ("kind_6", vec![6])];
        let heartbeat = inputs["raft_heartbeat"];
        refused.push(("raft_heartbeat_trailing", [heartbeat, &[0]].concat()));
        // A heartbeat body is its kind and then the commit index.
        let mut body = heartbeat.to_vec();
        body[heartbeat.len() - 9] = 10;
        refused.push(("raft_body_10", body));
        // The link of `raft_chain` sits between its count and the heartbeat body.
        let chain = inputs["raft_chain"];
        let links = inputs["raft_proof"].len() - 9;
        let link = link();
        let mut bytes = Vec::new();
        put_position(link.at, &mut bytes);
        put_change(&link.change, &mut bytes);
        assert_eq!(chain.len(), links + bytes.len() + 9);
        refused.push(("raft_chain_count_no_link", chain[..links].to_vec()));
        let cut = inputs["raft_chain_cut_in_link"];
        assert!(links < cut.len() && cut.len() < links + bytes.len());
        refused.push(("raft_chain_cut_in_link", chain[..cut.len()].to_vec()));
        for (name, want) in refused {
            assert_eq!(inputs[name], want, "{name}");
            assert_eq!(round_trip_message(inputs[name]), None, "{name}");
        }
    }
}
