//! Entry points for the fuzz targets. Needs the `sim` feature.

use crate::change::Change;
use crate::entry;
use crate::log;
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

/// The bytes that the mesh log record in `bytes` encodes to, or `None` when `bytes`
/// is not one valid record: a header that passes its check, the format version that
/// this build writes, a body of the length that the header claims that passes its
/// check, and a body that holds an optional hard state, then entries. It does not
/// check the number of the record or that its entries follow a log, which an open
/// of the log also checks.
#[must_use]
pub fn round_trip_log_record(bytes: &[u8]) -> Option<Vec<u8>> {
    let (number, hard, entries) = log::decode(bytes)?;
    Some(log::encode(number, hard, &entries))
}

/// Writes into `bytes` the length and the two checks of a mesh log record: the body
/// length as the count of bytes after the header, the body check over those bytes,
/// then the header check over the rest of the header. Does nothing to `bytes` shorter
/// than a record header.
pub fn seal_log_record(bytes: &mut [u8]) {
    log::seal(bytes);
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ops::Range;

    use raft::{
        Answer, Body, Data, Entry, Grant, Hard, Link, Position, Proof, Signature, Term,
        Voters,
    };

    use super::*;
    use types::digest::Digest;

    use crate::bytes::{put_change, put_position};
    use crate::change::{CHUNKS_MAX, Malformed};
    use crate::common::key;
    use spec::Pointer;

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

    #[test]
    fn each_spec_change_input_is_what_its_name_says() {
        let inputs = inputs!(
            "mesh_change": "spec",
            "spec_chunks_0",
            "spec_chunks_1024",
            "spec_chunks_1025",
            "spec_held",
            "spec_held_chunks_1024",
            "spec_holders_out_of_order",
        );
        let digest = |at: usize| {
            let mut bytes = [0; 32];
            bytes[30..].copy_from_slice(&u16::try_from(at).unwrap().to_be_bytes());
            Digest(bytes)
        };
        let spec = |chunks| Change::Spec {
            base: Pointer {
                version: 1,
                root: Digest([2; 32]),
            },
            root: Digest([3; 32]),
            chunks,
            holders: [key(1)].into(),
        };
        for (name, chunks) in [
            ("spec_chunks_0", BTreeSet::new()),
            ("spec_held", [Digest([4; 32]), Digest([5; 32])].into()),
            (
                "spec_held_chunks_1024",
                (0..CHUNKS_MAX).map(digest).collect(),
            ),
        ] {
            assert_eq!(Change::decode(inputs[name]), Ok(spec(chunks)), "{name}");
            let round_trip = round_trip_change(inputs[name]);
            assert_eq!(round_trip.as_deref(), Some(inputs[name]), "{name}");
        }
        // These two have the byte form before the holders, which ends where the count
        // of holders starts.
        for (name, held_name) in [
            ("spec", "spec_held"),
            ("spec_chunks_1024", "spec_held_chunks_1024"),
        ] {
            let mut held = inputs[name].to_vec();
            held.extend(1_u16.to_le_bytes());
            held.extend(key(1).as_u128().to_le_bytes());
            assert_eq!(held, inputs[held_name], "{name}");
        }
        // The chunk count is after the kind, the base, and the root.
        let mut over = inputs["spec_chunks_1024"].to_vec();
        over[73..75].copy_from_slice(&1025_u16.to_le_bytes());
        over.extend(digest(CHUNKS_MAX).0);
        assert_eq!(inputs["spec_chunks_1025"], over);
        for name in [
            "spec",
            "spec_chunks_1024",
            "spec_chunks_1025",
            "spec_holders_out_of_order",
        ] {
            let length = inputs[name].len();
            let body = Malformed::Body { kind: 4, length };
            assert_eq!(Change::decode(inputs[name]), Err(body), "{name}");
            assert_eq!(round_trip_change(inputs[name]), None, "{name}");
        }
        let mut order = spec(BTreeSet::new());
        let Change::Spec { holders, .. } = &mut order else {
            unreachable!()
        };
        *holders = [key(1), key(2)].into();
        let mut bytes = Vec::new();
        order.encode(&mut bytes);
        bytes[77..].rotate_left(16);
        assert_eq!(inputs["spec_holders_out_of_order"], bytes);
    }

    #[test]
    fn each_spec_change_input_with_a_repeated_or_falling_key_does_not_decode() {
        let inputs = inputs!(
            "mesh_change": "spec_chunks_equal",
            "spec_chunks_falling",
            "spec_holders_equal",
        );
        let encoded = |chunks: &[u8], holders: &[u8]| {
            let mut bytes = Vec::new();
            Change::Spec {
                base: Pointer {
                    version: 1,
                    root: Digest([2; 32]),
                },
                root: Digest([3; 32]),
                chunks: chunks.iter().map(|&chunk| Digest([chunk; 32])).collect(),
                holders: holders.iter().map(|&holder| key(holder)).collect(),
            }
            .encode(&mut bytes);
            bytes
        };
        // Each chunk is 32 bytes from byte 75.
        let mut equal = encoded(&[4, 5], &[]);
        let mut falling = equal.clone();
        equal[107..139].fill(4);
        assert_eq!(inputs["spec_chunks_equal"], equal);
        falling[75..139].rotate_left(32);
        assert_eq!(inputs["spec_chunks_falling"], falling);
        // The count of holders is at byte 107, after one chunk.
        let mut twice = encoded(&[5], &[3]);
        twice[107..109].copy_from_slice(&2_u16.to_le_bytes());
        twice.extend(key(3).as_u128().to_le_bytes());
        assert_eq!(inputs["spec_holders_equal"], twice);
        for (name, bytes) in inputs {
            let body = Malformed::Body {
                kind: 4,
                length: bytes.len(),
            };
            assert_eq!(Change::decode(bytes), Err(body), "{name}");
            assert_eq!(round_trip_change(bytes), None, "{name}");
        }
    }

    #[test]
    fn each_holder_bound_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_change": "spec_holders_64", "spec_holders_65");
        let at_bound = Change::Spec {
            base: Pointer {
                version: 1,
                root: Digest([2; 32]),
            },
            root: Digest([3; 32]),
            chunks: BTreeSet::new(),
            holders: (1..=64).map(key).collect(),
        };
        let mut bytes = Vec::new();
        at_bound.encode(&mut bytes);
        assert_eq!(inputs["spec_holders_64"], bytes);
        assert_eq!(Change::decode(&bytes), Ok(at_bound));
        // The count of holders is after the count of chunks, which is 0.
        bytes[75..77].copy_from_slice(&65_u16.to_le_bytes());
        bytes.extend(key(65).as_u128().to_le_bytes());
        assert_eq!(inputs["spec_holders_65"], bytes);
        let body = Malformed::Body {
            kind: 4,
            length: bytes.len(),
        };
        assert_eq!(Change::decode(&bytes), Err(body));
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

    /// The bytes of a mesh log record header.
    const HEADER: usize = 34;

    // The bytes of each header field, and of each hard state field in the body of
    // `hard_full`.
    const HEADER_FIELDS: [(&str, Range<usize>); 5] = [
        ("header_check", 0..8),
        ("version", 8..10),
        ("number", 10..18),
        ("length", 18..26),
        ("body_check", 26..34),
    ];
    const HARD_FIELDS: [(&str, Range<usize>); 8] = [
        ("tag", 0..1),
        ("term", 1..9),
        ("vote_presence", 9..10),
        ("vote_key", 10..26),
        ("leader_presence", 26..27),
        ("leader_key", 27..43),
        ("proof_presence", 43..44),
        ("proof", 44..229),
    ];

    /// A sealed record of `version` and `number` around `body`.
    fn record(version: u16, number: u64, body: &[u8]) -> Vec<u8> {
        let mut record = vec![0; 8];
        record.extend(version.to_le_bytes());
        record.extend(number.to_le_bytes());
        record.extend([0; 16]);
        record.extend(body);
        seal_log_record(&mut record);
        record
    }

    fn hard(full: bool) -> Hard {
        Hard {
            term: Term(5),
            vote: full.then(|| key(1)),
            leader: full.then(|| key(2)),
            proof: full.then(proof),
        }
    }

    /// Each input of `mesh_log`, built from values, and whether it is valid.
    fn log_inputs() -> Vec<(String, Vec<u8>, bool)> {
        let full = log::encode(3, Some(hard(true)), &entries());
        let body = &full[HEADER..];
        assert_eq!(record(1, 3, body), full);
        let mut inputs = vec![
            ("no_hard".into(), log::encode(0, None, &[]), true),
            (
                "no_hard_entries".into(),
                log::encode(1, None, &entries()),
                true,
            ),
            (
                "hard_term".into(),
                log::encode(2, Some(hard(false)), &[]),
                true,
            ),
            ("hard_full".into(), full.clone(), true),
        ];
        for (field, range) in HEADER_FIELDS {
            let cut = full[..range.start.midpoint(range.end)].to_vec();
            inputs.push((format!("cut_in_{field}"), cut, false));
        }
        let hard_only = log::encode(3, Some(hard(true)), &[]);
        let proof = &HARD_FIELDS.last().unwrap().1;
        assert_eq!(proof.end, hard_only[HEADER..].len());
        for (field, range) in HARD_FIELDS {
            let cut = record(1, 3, &body[..range.start.midpoint(range.end)]);
            inputs.push((format!("body_cut_in_{field}"), cut, false));
        }
        inputs.push(("version_2".into(), record(2, 3, body), false));
        let mut tag = body.to_vec();
        tag[0] = 2;
        inputs.push(("hard_tag_2".into(), record(1, 3, &tag), false));
        let mut cut = full.split_last().unwrap().1.to_vec();
        seal_log_record(&mut cut);
        inputs.push(("body_cut_in_entries".into(), cut, false));
        let mut after = [&full[..], &[0]].concat();
        seal_log_record(&mut after);
        inputs.push(("byte_after_entries".into(), after, false));
        inputs
    }

    #[test]
    fn each_log_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_log":
            "no_hard",
            "no_hard_entries",
            "hard_term",
            "hard_full",
            "cut_in_header_check",
            "cut_in_version",
            "cut_in_number",
            "cut_in_length",
            "cut_in_body_check",
            "body_cut_in_tag",
            "body_cut_in_term",
            "body_cut_in_vote_presence",
            "body_cut_in_vote_key",
            "body_cut_in_leader_presence",
            "body_cut_in_leader_key",
            "body_cut_in_proof_presence",
            "body_cut_in_proof",
            "version_2",
            "hard_tag_2",
            "body_cut_in_entries",
            "byte_after_entries",
        );
        assert_eq!(count_files("mesh_log"), inputs.len());
        let built = log_inputs();
        assert_eq!(built.len(), inputs.len());
        for (name, want, valid) in built {
            assert_eq!(inputs[name.as_str()], want, "{name}");
            let round_trip = round_trip_log_record(&want);
            assert_eq!(round_trip.as_deref(), valid.then_some(&want[..]), "{name}");
            let mut sealed = want.clone();
            seal_log_record(&mut sealed);
            assert_eq!(sealed, want, "{name} is not sealed");
        }
    }

    #[test]
    fn a_log_record_whose_check_fails_gives_none() {
        let mut record = log::encode(0, Some(hard(true)), &entries());
        assert_eq!(round_trip_log_record(&record).as_deref(), Some(&record[..]));
        let last = record.len() - 1;
        record[last] ^= 1;
        assert_eq!(round_trip_log_record(&record), None);
        seal_log_record(&mut record);
        assert_eq!(round_trip_log_record(&record).as_deref(), Some(&record[..]));
        record[12] ^= 1;
        assert_eq!(round_trip_log_record(&record), None);
    }

    #[test]
    fn a_log_record_with_bytes_after_it_gives_none() {
        let record = log::encode(0, Some(hard(true)), &entries());
        let two = [&record[..], &record[..]].concat();
        assert_eq!(round_trip_log_record(&two), None);
    }

    #[test]
    fn seal_writes_the_length_of_the_body() {
        let record = log::encode(0, Some(hard(true)), &entries());
        let mut sealed = record.clone();
        sealed[18..26].fill(0xFF);
        seal_log_record(&mut sealed);
        assert_eq!(sealed, record);
    }

    #[test]
    fn seal_does_nothing_to_less_than_a_header() {
        let record = log::encode(0, None, &[]);
        let mut short = record[..HEADER - 1].to_vec();
        short[0] ^= 1;
        let before = short.clone();
        seal_log_record(&mut short);
        assert_eq!(short, before);
    }
}
