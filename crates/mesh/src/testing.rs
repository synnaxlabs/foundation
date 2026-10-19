//! Entry points for the fuzz targets. Needs the `sim` feature.

use std::ops::Range;
use std::path::PathBuf;

use raft::{Entry, Hard};

use crate::change::Change;
use crate::driver::used;
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

/// Writes the checks of each record in each file that `dir` frames, as
/// [`scan_log_dir`] reads them. From the start of a file, at each header that is not
/// zeros, it writes the body check over the length that the header claims, then the
/// header check, and goes on to the next record. A body check of zeros stays, so that
/// record is torn. It stops at the end of a file, at a header of zeros, and after the
/// header check of a record that claims more bytes than the file holds.
#[expect(clippy::missing_panics_doc, reason = "each frame is in `dir`")]
pub fn seal_log_dir(dir: &mut [u8]) {
    for (_, body) in frames(dir) {
        log::seal_file(dir.get_mut(body).expect("invariant: a frame is in `dir`"));
    }
}

/// What an open reads from a log directory, and the log files that it leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scan {
    /// The last hard state written, or the default with none.
    pub hard: Hard,
    /// The entries from index 1.
    pub entries: Vec<Entry>,
    /// The log files that the open leaves, in the framing of [`scan_log_dir`]: each
    /// file in number order, named `log-<n>`, and no file after the one that holds the
    /// end. That file has zeros after its last record, and each file before it is as
    /// read. With no log file, it holds none.
    pub files: Vec<u8>,
}

/// What an open reads from the log directory that `dir` frames, and the files that it
/// leaves. `dir` is each file one after another: a byte of name length, the name, a
/// 2-byte little-endian body length, then the body. A file cut short ends with the
/// input. A name that is not UTF-8 reads with U+FFFD.
///
/// # Errors
///
/// What an open gives for these files: [`log::Error::Stray`], [`log::Error::Corrupt`],
/// or [`log::Error::Version`].
pub fn scan_log_dir(dir: &[u8]) -> Result<Scan, log::Error> {
    let (stored, left) = log::open_files(&unframe(dir))?;
    Ok(Scan {
        hard: stored.hard,
        entries: stored.entries,
        files: frame(&left),
    })
}

/// The name of the file in the folder `spec` of a mesh directory that names the
/// pointer that `name` names, or `None` when `name` names no pointer.
#[must_use]
pub fn round_trip_spec_name(name: &[u8]) -> Option<Vec<u8>> {
    let pointer = used::pointer(std::str::from_utf8(name).ok()?)?;
    Some(used::file(pointer).into_os_string().into_encoded_bytes())
}

// The path and the bytes of each file that `dir` frames.
fn unframe(dir: &[u8]) -> Vec<(PathBuf, Vec<u8>)> {
    let get = |range| dir.get(range).expect("invariant: a frame is in `dir`");
    frames(dir)
        .into_iter()
        .map(|(name, body)| {
            let name = String::from_utf8_lossy(get(name)).into_owned();
            (PathBuf::from(name), get(body).to_vec())
        })
        .collect()
}

// The framing of `files`, each named `log-<n>` for its place.
fn frame(files: &[Vec<u8>]) -> Vec<u8> {
    let mut dir = Vec::new();
    for (number, bytes) in files.iter().enumerate() {
        let name = format!("log-{number}");
        dir.push(u8::try_from(name.len()).expect("invariant: a log name is short"));
        dir.extend(name.as_bytes());
        let len = u16::try_from(bytes.len())
            .expect("invariant: an open leaves no file longer than it read");
        dir.extend(len.to_le_bytes());
        dir.extend(bytes);
    }
    dir
}

// The name and the body of each file that `dir` frames, as ranges of `dir`.
fn frames(dir: &[u8]) -> Vec<(Range<usize>, Range<usize>)> {
    let span = |from: usize, len: usize| from..from.saturating_add(len).min(dir.len());
    let mut frames = Vec::new();
    let mut at = 0;
    while let Some(&len) = dir.get(at) {
        let name = span(at.saturating_add(1), usize::from(len));
        let length = span(name.end, 2);
        let len = match dir.get(length.clone()) {
            Some(&[low, high]) => u16::from_le_bytes([low, high]),
            _ => 0,
        };
        let body = span(length.end, usize::from(len));
        at = body.end;
        frames.push((name, body));
    }
    frames
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use raft::{Answer, Body, Data, Grant, Link, Proof, Signature, Term, Voters};
    use spec::Pointer;

    use super::*;
    use types::channel;
    use types::digest::Digest;
    use types::time::Stamp;

    use crate::bytes::{put_change, put_count, put_name, put_position};
    use crate::card;
    use crate::change::{self, CHUNKS_MAX, Malformed, Undecoded};
    use crate::common::{at, index, key, member, name, public, ticket, unchecked};
    use crate::message::{FOUNDING_BYTES_MAX, VOTERS_MAX};
    use crate::refusal::Refusal;
    use crate::region::{self, Request, Unfit};

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
            homes: BTreeMap::new(),
        };
        // These three have the byte form before the homes, which ends where the count
        // of homes starts.
        for (name, chunks) in [
            ("spec_chunks_0", BTreeSet::new()),
            ("spec_held", [Digest([4; 32]), Digest([5; 32])].into()),
            (
                "spec_held_chunks_1024",
                (0..CHUNKS_MAX).map(digest).collect(),
            ),
        ] {
            let homed = homed(inputs[name]);
            assert_eq!(Change::decode(&homed), Ok(spec(chunks)), "{name}");
            assert_eq!(round_trip_change(&homed), Some(homed), "{name}");
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
        for (name, bytes) in &inputs {
            let body = Malformed::Body(Undecoded {
                kind: 4,
                length: bytes.len(),
            });
            assert_eq!(Change::decode(bytes), Err(body), "{name}");
            assert_eq!(round_trip_change(bytes), None, "{name}");
        }
        let mut order = spec(BTreeSet::new());
        let Change::Spec { holders, .. } = &mut order else {
            unreachable!()
        };
        *holders = [key(1), key(2)].into();
        let mut bytes = unhomed(&order);
        bytes[77..].rotate_left(16);
        assert_eq!(inputs["spec_holders_out_of_order"], bytes);
    }

    #[test]
    fn each_card_change_input_is_what_its_name_says() {
        let inputs =
            inputs!("mesh_change": "card", "card_forged", "card_short", "kind_6");
        let encoded = |change: &Change| {
            let mut bytes = Vec::new();
            change.encode(&mut bytes);
            bytes
        };
        let mut card =
            crate::common::unchecked(&crate::common::signed(3, "plant.edge.a"));
        let change = Change::Card(Box::new(card.clone()));
        assert_eq!(inputs["card"], encoded(&change));
        card.signature[0] ^= 1;
        let forged = Change::Card(Box::new(card));
        assert_eq!(inputs["card_forged"], encoded(&forged));
        for (name, change) in [("card", change), ("card_forged", forged)] {
            let bytes = inputs[name];
            assert_eq!(Change::decode(bytes), Ok(change), "{name}");
            assert_eq!(round_trip_change(bytes), Some(bytes.to_vec()), "{name}");
        }
        // The cut is inside the name of the card, after the kind and the node key.
        let short = &inputs["card"][..21];
        assert_eq!(inputs["card_short"], short);
        let body = Malformed::Body(Undecoded {
            kind: 5,
            length: 21,
        });
        assert_eq!(Change::decode(short), Err(body));
        let home = include_bytes!("../../../oracles/fuzz/mesh_change/home");
        let mut other = home.to_vec();
        other[0] = 6;
        assert_eq!(inputs["kind_6"], other);
        let unknown = change::Unknown::Kind { kind: 6 };
        assert_eq!(Change::decode(&other), Err(unknown.into()));
        assert_eq!(round_trip_change(&other), None);
    }

    // Each decodes as its input before the homes does with the count added, which
    // `each_spec_change_input_is_what_its_name_says` and
    // `each_holder_bound_input_is_what_its_name_says` check.
    #[test]
    fn each_input_with_a_count_of_0_homes_is_its_input_before_the_homes_with_it() {
        let before = inputs!(
            "mesh_change": "spec_chunks_0",
            "spec_held",
            "spec_held_chunks_1024",
            "spec_holders_64",
        );
        let counted = inputs!(
            "mesh_change": "spec_chunks_0_homes_0",
            "spec_homes_0",
            "spec_held_chunks_1024_homes_0",
            "spec_holders_64_homes_0",
        );
        for (name, counted_name) in [
            ("spec_chunks_0", "spec_chunks_0_homes_0"),
            ("spec_held", "spec_homes_0"),
            ("spec_held_chunks_1024", "spec_held_chunks_1024_homes_0"),
            ("spec_holders_64", "spec_holders_64_homes_0"),
        ] {
            assert_eq!(counted[counted_name], homed(before[name]), "{name}");
        }
    }

    // With the counts that its byte form lacks, each fails for what its name says.
    #[test]
    fn each_refused_spec_change_input_fails_for_what_its_name_says() {
        let inputs =
            inputs!("mesh_change": "spec_chunks_1025", "spec_holders_out_of_order");
        for (name, lacked) in
            [("spec_chunks_1025", 4), ("spec_holders_out_of_order", 2)]
        {
            let whole = [inputs[name], &vec![0; lacked]].concat();
            let body = Malformed::Body(Undecoded {
                kind: 4,
                length: whole.len(),
            });
            assert_eq!(Change::decode(&whole), Err(body), "{name}");
        }
    }

    #[test]
    fn each_spec_change_input_with_a_repeated_or_falling_key_does_not_decode() {
        let inputs = inputs!(
            "mesh_change": "spec_chunks_equal",
            "spec_chunks_falling",
            "spec_holders_equal",
        );
        let encoded = |chunks: &[u8], holders: &[u8]| {
            unhomed(&Change::Spec {
                base: Pointer {
                    version: 1,
                    root: Digest([2; 32]),
                },
                root: Digest([3; 32]),
                chunks: chunks.iter().map(|&chunk| Digest([chunk; 32])).collect(),
                holders: holders.iter().map(|&holder| key(holder)).collect(),
                homes: BTreeMap::new(),
            })
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
            let homed = homed(bytes);
            let body = Malformed::Body(Undecoded {
                kind: 4,
                length: homed.len(),
            });
            assert_eq!(Change::decode(&homed), Err(body), "{name}");
            assert_eq!(round_trip_change(&homed), None, "{name}");
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
            homes: BTreeMap::new(),
        };
        let mut bytes = unhomed(&at_bound);
        assert_eq!(inputs["spec_holders_64"], bytes);
        assert_eq!(Change::decode(&homed(&bytes)), Ok(at_bound));
        // The count of holders is after the count of chunks, which is 0.
        bytes[75..77].copy_from_slice(&65_u16.to_le_bytes());
        bytes.extend(key(65).as_u128().to_le_bytes());
        assert_eq!(inputs["spec_holders_65"], bytes);
        let homed = homed(&bytes);
        let body = Malformed::Body(Undecoded {
            kind: 4,
            length: homed.len(),
        });
        assert_eq!(Change::decode(&homed), Err(body));
    }

    #[test]
    fn each_homed_spec_change_input_is_what_its_name_says() {
        let inputs = inputs!(
            "mesh_change": "spec_held",
            "spec_homed",
            "spec_homes_512",
            "spec_homes_513",
            "spec_homes_equal",
            "spec_homes_falling",
        );
        // The byte form of `spec_held` with each home of `homes`, in order.
        let homed = |homes: &[(u128, u128)]| {
            let mut bytes = inputs["spec_held"].to_vec();
            bytes.extend(u16::try_from(homes.len()).unwrap().to_le_bytes());
            for (index, home) in homes {
                bytes.extend(index.to_le_bytes());
                bytes.extend(home.to_le_bytes());
            }
            bytes
        };
        let change = |homes: &[(u128, u128)]| Change::Spec {
            base: Pointer {
                version: 1,
                root: Digest([2; 32]),
            },
            root: Digest([3; 32]),
            chunks: [Digest([4; 32]), Digest([5; 32])].into(),
            holders: [key(1)].into(),
            homes: homes
                .iter()
                .map(|&(index, home)| {
                    (
                        channel::Key::from_u128(index),
                        key(u8::try_from(home).unwrap()),
                    )
                })
                .collect(),
        };
        let at_bound: Vec<_> = (1..=512).map(|index| (index, 2)).collect();
        for (name, homes) in
            [("spec_homed", &[(5, 3)][..]), ("spec_homes_512", &at_bound)]
        {
            let bytes = homed(homes);
            assert_eq!(inputs[name], bytes, "{name}");
            assert_eq!(Change::decode(&bytes), Ok(change(homes)), "{name}");
            assert_eq!(round_trip_change(&bytes), Some(bytes), "{name}");
        }
        let over: Vec<_> = (1..=513).map(|index| (index, 2)).collect();
        for (name, homes) in [
            ("spec_homes_513", &over[..]),
            ("spec_homes_equal", &[(5, 2), (5, 3)]),
            ("spec_homes_falling", &[(9, 2), (5, 3)]),
        ] {
            let bytes = homed(homes);
            assert_eq!(inputs[name], bytes, "{name}");
            let body = Malformed::Body(Undecoded {
                kind: 4,
                length: bytes.len(),
            });
            assert_eq!(Change::decode(&bytes), Err(body), "{name}");
            assert_eq!(round_trip_change(&bytes), None, "{name}");
        }
    }

    // The byte form of a spec change with no home, before the homes: with no count of
    // homes.
    fn unhomed(change: &Change) -> Vec<u8> {
        let mut bytes = Vec::new();
        change.encode(&mut bytes);
        assert_eq!([bytes.pop(), bytes.pop()], [Some(0), Some(0)]);
        bytes
    }

    // The byte form `unhomed` gave, with a count of 0 homes.
    fn homed(unhomed: &[u8]) -> Vec<u8> {
        [unhomed, &[0, 0]].concat()
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
    fn messages() -> [(&'static str, Message); 19] {
        let raft = |body| message(body, None, Vec::new());
        let heartbeat = || Body::Heartbeat { commit: 4 };
        let home = include_bytes!("../../../oracles/fuzz/mesh_change/home");
        let card = member(2).card;
        let request = Box::new(Request {
            ticket: public(1),
            card: unchecked(&card),
            admission: ticket(1).admission(&card),
            status: BTreeSet::from(["disk".parse().unwrap()]),
        });
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
            ("join", Message::Join { request }),
            ("joined", Message::Joined { bytes: 300 }),
            (
                "unjoined",
                Message::Unjoined {
                    refusal: Refusal::NoVote,
                },
            ),
            ("founding", Message::Founding { part: vec![7; 3] }),
        ]
    }

    /// The answer of each kind of refusal of a join, by the name of its input.
    fn unjoined() -> impl Iterator<Item = (&'static str, Message)> {
        let refused = region_refusals()
            .map(|(input, refused)| (input, Refusal::Refused(refused)));
        let others = [
            ("unjoined_contested", Refusal::Contested),
            ("unjoined_unsynced", Refusal::Unsynced),
            ("unjoined_unled", Refusal::Unled),
            ("unjoined_stopped", Refusal::Stopped),
        ];
        refused
            .chain(others)
            .map(|(input, refusal)| (input, Message::Unjoined { refusal }))
    }

    /// Each kind of [`region::Refused`] of a join, by the name of its input.
    fn region_refusals() -> impl Iterator<Item = (&'static str, region::Refused)> {
        let unfit = region::Refused::Unfit;
        let node = name("plant.node2");
        [
            (
                "unjoined_forged",
                region::Refused::Forged(card::Forged { node: key(2) }),
            ),
            (
                "unjoined_reserved",
                unfit(Unfit::Reserved { name: name("@b") }),
            ),
            (
                "unjoined_outside",
                unfit(Unfit::Outside {
                    name: name("works.node2"),
                    region: name("plant").into(),
                }),
            ),
            (
                "unjoined_long",
                unfit(Unfit::Long {
                    name: node.clone(),
                    status: name("plant.node2.disk"),
                }),
            ),
            (
                "unjoined_duplicate",
                unfit(Unfit::Duplicate { key: key(2) }),
            ),
            ("unjoined_held", unfit(Unfit::Held { key: key(2) })),
            (
                "unjoined_taken",
                unfit(Unfit::Taken {
                    name: node,
                    key: key(3),
                }),
            ),
            ("unjoined_reused", unfit(Unfit::Reused { key: index(5) })),
            (
                "unjoined_unknown",
                region::Refused::Unknown {
                    public_key: public(1),
                },
            ),
        ]
        .into_iter()
        .chain(
            ticket_refusals()
                .map(|(input, refused)| (input, region::Refused::Ticket(refused))),
        )
    }

    /// Each kind of [`crate::ticket::Refused`], by the name of its input.
    fn ticket_refusals() -> impl Iterator<Item = (&'static str, crate::ticket::Refused)>
    {
        [
            (
                "unjoined_ticket_forged",
                (crate::ticket::Refused::Forged {
                    node: key(2),
                    public_key: public(1),
                }),
            ),
            (
                "unjoined_scope",
                (crate::ticket::Refused::Scope {
                    name: name("works.node2"),
                    prefix: name("plant"),
                }),
            ),
            (
                "unjoined_expired",
                (crate::ticket::Refused::Expired {
                    public_key: public(1),
                    expiry: Stamp::from_nanos(5),
                    at: Stamp::from_nanos(6),
                }),
            ),
            (
                "unjoined_used",
                (crate::ticket::Refused::Used {
                    public_key: public(1),
                }),
            ),
        ]
        .into_iter()
    }

    /// The join of node 2 by ticket 1, with the status names `names`.
    fn join_with(names: impl IntoIterator<Item = String>) -> Message {
        let card = member(2).card;
        Message::Join {
            request: Box::new(Request {
                ticket: public(1),
                card: unchecked(&card),
                admission: ticket(1).admission(&card),
                status: names.into_iter().map(|text| name(&text)).collect(),
            }),
        }
    }

    /// A join of two status names, with the last name first.
    fn join_out_of_order() -> Vec<u8> {
        let names = ["plant.node2.a", "plant.node2.b"];
        let mut bytes = join_with(names.map(String::from)).encode();
        let mut swapped = Vec::new();
        for text in names.iter().rev() {
            put_name(&name(text), &mut swapped);
        }
        let start = bytes.len().checked_sub(swapped.len()).unwrap();
        bytes[start..].copy_from_slice(&swapped);
        bytes
    }

    /// The inputs of `mesh_message`, by name.
    fn message_inputs() -> BTreeMap<&'static str, &'static [u8]> {
        inputs!("mesh_message":
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
            "raft_append_entry_over_bytes_max",
            "raft_proof_over_grants_max",
            "join",
            "joined",
            "unjoined",
            "founding",
            "joined_over_founding_bytes_max",
            "unjoined_forged",
            "unjoined_reserved",
            "unjoined_outside",
            "unjoined_long",
            "unjoined_duplicate",
            "unjoined_held",
            "unjoined_taken",
            "unjoined_reused",
            "unjoined_unknown",
            "unjoined_ticket_forged",
            "unjoined_scope",
            "unjoined_expired",
            "unjoined_used",
            "unjoined_contested",
            "unjoined_unsynced",
            "unjoined_unled",
            "unjoined_stopped",
            "join_status_65",
            "join_status_out_of_order",
            "kind_10",
        )
    }

    #[test]
    fn each_message_input_is_what_its_name_says() {
        let inputs = message_inputs();
        assert_eq!(count_files("mesh_message"), inputs.len());
        for (name, want) in messages().into_iter().chain(unjoined()) {
            assert_eq!(Message::decode(inputs[name]), Some(want), "{name}");
            let round_trip = round_trip_message(inputs[name]);
            assert_eq!(round_trip.as_deref(), Some(inputs[name]), "{name}");
        }

        // `kind_6` is a join cut after its kind, and 10 is the first kind of no
        // message.
        let mut refused = vec![
            ("none", vec![]),
            ("kind_6", vec![6]),
            ("kind_10", vec![10]),
            ("join_status_out_of_order", join_out_of_order()),
        ];
        let names = (0..65).map(|at| format!("plant.node2.s{at}"));
        refused.push(("join_status_65", join_with(names).encode()));
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
        refused.extend(over_bounds());
        for (name, want) in refused {
            assert_eq!(inputs[name], want, "{name}");
            assert_eq!(round_trip_message(inputs[name]), None, "{name}");
        }
    }

    // The message inputs that a bound of the decode refuses.
    fn over_bounds() -> [(&'static str, Vec<u8>); 3] {
        let over = Entry {
            at: at(3, 4),
            data: Data::Bytes(vec![7; change::BYTES_MAX + 1]),
        };
        let body = Body::Append {
            prev: at(2, 3),
            entries: vec![over],
            commit: 4,
        };
        let entry = message(body, None, Vec::new()).encode();
        let grants = u8::try_from(2 * VOTERS_MAX + 1).unwrap();
        let grant = |voter| (key(voter), Some(Signature([7; 64])));
        let over = Proof {
            voters: (1..=grants).map(grant).collect(),
            ..proof()
        };
        let body = Body::Heartbeat { commit: 4 };
        let proof = message(body, Some(over), Vec::new()).encode();
        let mut joined = Message::Joined { bytes: 0 }.encode();
        joined.truncate(1);
        put_count(FOUNDING_BYTES_MAX + 1, &mut joined);
        [
            ("raft_append_entry_over_bytes_max", entry),
            ("raft_proof_over_grants_max", proof),
            ("joined_over_founding_bytes_max", joined),
        ]
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

    /// A log file of `len` bytes that holds `records` back to back from its start.
    fn log_file(records: &[&[u8]], len: usize) -> Vec<u8> {
        let mut file = records.concat();
        assert!(file.len() <= len);
        file.resize(len, 0);
        file
    }

    /// The framing of `files`, each a name and its bytes.
    fn framed(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, bytes) in files {
            out.push(u8::try_from(name.len()).unwrap());
            out.extend(name.as_bytes());
            out.extend(u16::try_from(bytes.len()).unwrap().to_le_bytes());
            out.extend(*bytes);
        }
        out
    }

    fn empty(index: u64) -> Entry {
        Entry {
            at: at(1, index),
            data: Data::Empty,
        }
    }

    /// Record `number` with an empty entry at `index`.
    fn log_record(number: u64, index: u64) -> Vec<u8> {
        log::encode(number, None, &[empty(index)])
    }

    /// A scan that read `hard` and `entries`, and leaves `files`.
    fn scanned(hard: Hard, entries: Vec<Entry>, files: Vec<u8>) -> Scan {
        Scan {
            hard,
            entries,
            files,
        }
    }

    /// A scan that read no hard state and an empty entry at each index from 1 to
    /// `last`, and leaves `files`.
    fn empties(last: u64, files: Vec<u8>) -> Scan {
        scanned(Hard::default(), (1..=last).map(empty).collect(), files)
    }

    /// Record `number` with an empty entry at `index`, and a body check of zeros.
    fn torn(number: u64, index: u64) -> Vec<u8> {
        let mut record = log_record(number, index);
        record[26..HEADER].fill(0);
        log::seal_file(&mut record);
        record
    }

    fn corrupt(file: &str, offset: usize) -> log::Error {
        log::Error::Corrupt {
            path: Path::new("log").join(file),
            offset: offset.try_into().unwrap(),
        }
    }

    fn stray(file: &str) -> log::Error {
        log::Error::Stray {
            path: Path::new("log").join(file),
        }
    }

    type LogDirInput = (&'static str, Vec<u8>, Result<Scan, log::Error>);

    const LEN: usize = 1024;

    /// Record `number` with an empty entry at `number + 1`.
    fn numbered(number: u64) -> Vec<u8> {
        log_record(number, number.strict_add(1))
    }

    /// Each input of `mesh_log_dir`, built from values, and what its scan gives.
    fn log_dir_inputs() -> Vec<LogDirInput> {
        [record_inputs(), torn_inputs(), file_inputs(), name_inputs()].concat()
    }

    /// Inputs of the records of one file.
    fn record_inputs() -> Vec<LogDirInput> {
        let r0 = numbered(0);
        let len = r0.len();
        let record = log::encode(0, Some(hard(true)), &[empty(1)]);
        let one = log_file(&[&record], LEN);
        // A record that leaves less than a header before the first sector end, so the
        // next starts at the second sector.
        let filler = |len: usize| Entry {
            at: at(1, 1),
            data: Data::Bytes(vec![7; len]),
        };
        let short = log::encode(0, None, &[filler(0)]).len();
        let fill = log::encode(0, None, &[filler(490_usize.strict_sub(short))]);
        assert_eq!(fill.len(), 490);
        let mut sector = log_file(&[&fill], 512);
        let sector_entries = vec![filler(490_usize.strict_sub(short)), empty(2)];
        sector.extend(log_file(&[&log_record(1, 2)], 512));
        let version = as_version_2(r0.clone());
        let mut cut = log_file(&[&r0], LEN);
        cut.truncate(len.strict_sub(1));
        log::seal_file(&mut cut);
        let mut garbage = r0.clone();
        garbage.extend([1; 10]);
        let one_file = |bytes: &[u8]| framed(&[("log-0", bytes)]);
        vec![
            (
                "one_record",
                framed(&[("lock", &[]), ("log-0", &one)]),
                Ok(scanned(hard(true), vec![empty(1)], one_file(&one))),
            ),
            (
                "next_sector",
                one_file(&sector),
                Ok(scanned(Hard::default(), sector_entries, one_file(&sector))),
            ),
            (
                "number_gap",
                one_file(&log_file(&[&r0, &numbered(2)], LEN)),
                Err(corrupt("log-0", len)),
            ),
            (
                "version_2",
                one_file(&log_file(&[&version], LEN)),
                Err(version_2("log-0")),
            ),
            ("body_cut", one_file(&cut), Err(corrupt("log-0", 0))),
            (
                "entries_do_not_follow",
                one_file(&log_file(&[&log_record(0, 2)], LEN)),
                Err(corrupt("log-0", 0)),
            ),
            ("garbage", one_file(&garbage), Err(corrupt("log-0", len))),
        ]
    }

    /// Inputs of a torn record.
    fn torn_inputs() -> Vec<LogDirInput> {
        let [r0, r1, r2] = [0, 1, 2].map(numbered);
        let len = r0.len();
        let zeros = vec![0; LEN];
        let torn_end = log_file(&[&r0, &torn(1, 2)], LEN);
        let kept = log_file(&[&r0], LEN);
        vec![
            (
                "torn_end",
                framed(&[("log-0", &torn_end)]),
                Ok(empties(1, framed(&[("log-0", &kept)]))),
            ),
            (
                "torn_end_in_next_file",
                framed(&[("log-0", &kept), ("log-1", &log_file(&[&torn(1, 2)], LEN))]),
                Ok(empties(
                    1,
                    framed(&[("log-0", &kept), ("log-1", &[0; LEN])]),
                )),
            ),
            (
                "torn_not_next",
                framed(&[("log-0", &log_file(&[&r0, &torn(5, 2)], LEN))]),
                Err(corrupt("log-0", len)),
            ),
            (
                "after_torn",
                framed(&[("log-0", &log_file(&[&r0, &torn(1, 2), &r2], LEN))]),
                Err(corrupt("log-0", len)),
            ),
            (
                "torn_before_next_file",
                framed(&[("log-0", &torn_end), ("log-1", &log_file(&[&r2], LEN))]),
                Err(corrupt("log-0", len)),
            ),
            (
                "torn_end_before_next_record",
                framed(&[("log-0", &torn_end), ("log-1", &log_file(&[&r1], LEN))]),
                Err(corrupt("log-0", len)),
            ),
            (
                "torn_end_before_garbage_file",
                framed(&[
                    ("log-0", &log_file(&[&torn(0, 1)], LEN)),
                    ("log-1", &[1; 10]),
                ]),
                Err(corrupt("log-1", 0)),
            ),
            (
                "torn_first_before_empty_files",
                framed(&[
                    ("log-0", &log_file(&[&torn(0, 1)], LEN)),
                    ("log-1", &zeros),
                    ("log-2", &zeros),
                ]),
                Err(corrupt("log-1", 0)),
            ),
        ]
    }

    /// Inputs of the order of the files.
    fn file_inputs() -> Vec<LogDirInput> {
        let [r0, r1, r2] = [0, 1, 2].map(numbered);
        let two = log_file(&[&r0, &r1], LEN);
        let kept = log_file(&[&r0], LEN);
        let zeros = vec![0; LEN];
        // Bytes after the zero header that ends a file before the end file stay.
        let mut after_end = kept.clone();
        after_end[r0.len().strict_add(HEADER).strict_add(10)] = 9;
        let after_end_files =
            framed(&[("log-0", &after_end), ("log-1", &log_file(&[&r1], LEN))]);
        let version = as_version_2(log::encode(9, None, &[empty(2)]));
        vec![
            (
                "two_files",
                framed(&[("log-1", &log_file(&[&r2], LEN)), ("log-0", &two)]),
                Ok(empties(
                    3,
                    framed(&[("log-0", &two), ("log-1", &log_file(&[&r2], LEN))]),
                )),
            ),
            (
                "stale_next_file",
                framed(&[("log-0", &two), ("log-1", &log_file(&[&r1], LEN))]),
                Err(corrupt("log-1", 0)),
            ),
            (
                "next_file_past_next",
                framed(&[("log-0", &kept), ("log-1", &log_file(&[&r2], LEN))]),
                Err(corrupt("log-0", r0.len())),
            ),
            (
                "next_file_of_version_2",
                framed(&[("log-0", &kept), ("log-1", &log_file(&[&version], LEN))]),
                Err(version_2("log-1")),
            ),
            (
                "garbage_next_file",
                framed(&[("log-0", &kept), ("log-1", &[1; 10])]),
                Err(corrupt("log-1", 0)),
            ),
            (
                "bytes_after_a_file_end",
                after_end_files.clone(),
                Ok(empties(2, after_end_files)),
            ),
            (
                "empty_file_before_record",
                framed(&[("log-0", &zeros), ("log-1", &kept)]),
                Err(corrupt("log-0", 0)),
            ),
            (
                "empty_file_before_garbage",
                framed(&[("log-0", &zeros), ("log-1", &[1; 10])]),
                Err(corrupt("log-0", 0)),
            ),
            (
                "empty_files_before_record",
                framed(&[("log-0", &zeros), ("log-1", &zeros), ("log-2", &kept)]),
                Err(corrupt("log-0", 0)),
            ),
            (
                "empty_file_not_last",
                framed(&[("log-0", &kept), ("log-1", &zeros), ("log-2", &zeros)]),
                Err(corrupt("log-1", 0)),
            ),
            (
                "spare",
                framed(&[("log-0", &kept), ("log-1", &zeros)]),
                Ok(empties(1, framed(&[("log-0", &kept)]))),
            ),
        ]
    }

    /// Inputs of the names of the files and of the framing.
    fn name_inputs() -> Vec<LogDirInput> {
        vec![
            ("empty", Vec::new(), Ok(empties(0, Vec::new()))),
            ("lock", framed(&[("lock", &[])]), Ok(empties(0, Vec::new()))),
            (
                "stray",
                framed(&[("lock", &[]), ("other", &[])]),
                Err(stray("other")),
            ),
            (
                "leading_zero",
                framed(&[("log-01", &[])]),
                Err(stray("log-01")),
            ),
            ("past_count", framed(&[("log-1", &[])]), Err(stray("log-1"))),
            (
                "repeated",
                framed(&[("log-0", &[]), ("log-0", &[])]),
                Err(stray("log-0")),
            ),
            ("cut_in_name", vec![5, b'l', b'o'], Err(stray("lo"))),
            (
                "cut_in_length",
                [&framed(&[("log-0", &[])])[..6], &[16]].concat(),
                Ok(empties(0, framed(&[("log-0", &[])]))),
            ),
        ]
    }

    /// `record` with format version 2, sealed.
    fn as_version_2(mut record: Vec<u8>) -> Vec<u8> {
        record[8..10].copy_from_slice(&2_u16.to_le_bytes());
        log::seal_file(&mut record);
        record
    }

    fn version_2(file: &str) -> log::Error {
        log::Error::Version {
            path: Path::new("log").join(file),
            found: 2,
        }
    }

    #[test]
    fn each_log_dir_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_log_dir":
            "empty",
            "lock",
            "one_record",
            "two_files",
            "next_sector",
            "torn_end",
            "torn_not_next",
            "after_torn",
            "torn_before_next_file",
            "torn_end_before_next_record",
            "torn_end_before_garbage_file",
            "torn_first_before_empty_files",
            "stale_next_file",
            "next_file_past_next",
            "number_gap",
            "version_2",
            "body_cut",
            "entries_do_not_follow",
            "garbage",
            "empty_file_before_record",
            "empty_file_not_last",
            "spare",
            "torn_end_in_next_file",
            "next_file_of_version_2",
            "garbage_next_file",
            "empty_file_before_garbage",
            "empty_files_before_record",
            "bytes_after_a_file_end",
            "stray",
            "leading_zero",
            "past_count",
            "repeated",
            "cut_in_name",
            "cut_in_length",
        );
        assert_eq!(count_files("mesh_log_dir"), inputs.len());
        let built = log_dir_inputs();
        assert_eq!(built.len(), inputs.len());
        for (name, want, scanned) in built {
            assert_eq!(inputs[name], want, "{name}");
            let mut sealed = want.clone();
            seal_log_dir(&mut sealed);
            assert_eq!(sealed, want, "{name} is not sealed");
            assert_eq!(scan_log_dir(&want), scanned, "{name}");
        }
    }

    #[test]
    fn seal_log_dir_seals_each_record_of_each_file() {
        let [r0, r1] = [0, 1].map(|number| log_record(number, number.strict_add(1)));
        let sealed = framed(&[
            ("log-0", &log_file(&[&r0, &r1], 1024)),
            ("log-1", &log_file(&[&log_record(2, 3)], 1024)),
        ]);
        let mut dir = sealed.clone();
        // A byte of each record's body, in each file.
        for at in
            [8 + HEADER, 8 + r0.len() + HEADER, 8 + 1024 + 8 + HEADER].map(|at| at + 5)
        {
            dir[at] ^= 1;
        }
        assert_eq!(scan_log_dir(&dir), Err(corrupt("log-0", 0)));
        seal_log_dir(&mut dir);
        assert_ne!(dir, sealed);
        assert_eq!(scan_log_dir(&dir).map(|scan| scan.entries.len()), Ok(3));
    }

    #[test]
    fn a_file_of_the_largest_length_comes_back_whole() {
        let file = log_file(&[&log_record(0, 1)], usize::from(u16::MAX));
        let dir = framed(&[("log-0", &file)]);
        assert_eq!(scan_log_dir(&dir).map(|scan| scan.files), Ok(dir));
    }

    #[test]
    fn seal_log_dir_keeps_a_body_check_of_zeros() {
        let r0 = log_record(0, 1);
        let torn = torn(1, 2);
        let mut dir = framed(&[("log-0", &log_file(&[&r0, &torn], 1024))]);
        let before = dir.clone();
        seal_log_dir(&mut dir);
        assert_eq!(dir, before);
    }

    #[test]
    fn seal_log_dir_stops_after_the_header_of_a_short_body() {
        let r0 = log_record(0, 1);
        let mut file = log_file(&[&r0], 1024);
        file[18..26].copy_from_slice(&2000_u64.to_le_bytes());
        let mut dir = framed(&[("log-0", &file)]);
        seal_log_dir(&mut dir);
        let mut sealed = r0.clone();
        sealed[18..26].copy_from_slice(&2000_u64.to_le_bytes());
        let (claimed, rest) = sealed.split_at_mut(8);
        claimed.copy_from_slice(&crate::file::check(&rest[..HEADER - 8]));
        assert_eq!(&dir[8..8 + r0.len()], &sealed[..]);
        assert_eq!(scan_log_dir(&dir), Err(corrupt("log-0", 0)));
    }

    #[test]
    fn a_cut_frame_ends_with_the_input() {
        let name = [5, b'l', b'o', b'g', b'-', b'0'];
        let scan = |tail: &[u8]| scan_log_dir(&[&name[..], tail].concat());
        let zeros = |len| Ok(empties(0, framed(&[("log-0", &vec![0; len])])));
        assert_eq!(scan_log_dir(&[]), Ok(empties(0, Vec::new())));
        assert_eq!(scan_log_dir(&[5, b'l', b'o']), Err(stray("lo")));
        assert_eq!(scan(&[]), zeros(0));
        assert_eq!(scan(&[16]), zeros(0));
        assert_eq!(scan(&[16, 0, 0, 0, 0]), zeros(3));
        assert_eq!(scan(&[1, 0, 0, 0]), Err(stray("")));
    }

    #[test]
    fn a_name_that_is_not_utf8_reads_with_the_replacement_character() {
        let dir = [2, b'a', 0xFF, 0, 0];
        assert_eq!(scan_log_dir(&dir), Err(stray("a\u{FFFD}")));
    }

    fn refs(records: &[Vec<u8>]) -> Vec<&[u8]> {
        records.iter().map(Vec::as_slice).collect()
    }

    proptest::proptest! {
        // The oracle of `mesh_log_dir`: an open of the files that an open leaves reads
        // the same log, and the files are a fixed point after two opens.
        #[test]
        fn a_scan_of_the_files_a_scan_leaves_gives_them(
            records in 0_u64..6,
            split in 0_u64..6,
            tail in 0_u8..4,
            flip in proptest::option::of((0_usize..2048, 1_u8..=255)),
        ) {
            let split = split.min(records);
            let all: Vec<_> = (0..records).map(|n| log_record(n, n.strict_add(1))).collect();
            let (first, second) = all.split_at(usize::try_from(split).unwrap());
            let mut second = refs(second);
            let torn = torn(records, records.strict_add(1));
            if tail == 1 {
                second.push(&torn);
            }
            let mut files = vec![("log-0", log_file(&refs(first), 1024))];
            if tail != 2 {
                files.push(("log-1", log_file(&second, 1024)));
            }
            if tail == 3 {
                files.push(("log-2", vec![0; 1024]));
            }
            let named: Vec<(&str, &[u8])> =
                files.iter().map(|(name, bytes)| (*name, bytes.as_slice())).collect();
            let mut dir = framed(&named);
            if let Some((at, mask)) = flip {
                let at = at.strict_rem(dir.len());
                dir[at] ^= mask;
            }
            seal_log_dir(&mut dir);
            if let Ok(first) = scan_log_dir(&dir) {
                let second = scan_log_dir(&first.files).unwrap();
                let third = scan_log_dir(&second.files).unwrap();
                proptest::prop_assert_eq!(third, second.clone());
                proptest::prop_assert_eq!(
                    (second.hard, second.entries),
                    (first.hard, first.entries)
                );
            }
        }
    }

    #[test]
    fn each_spec_name_input_is_what_its_name_says() {
        let inputs = inputs!("mesh_spec_name":
            "pointer",
            "version_0",
            "upper_case",
            "short_root",
            "long_root",
            "odd_root",
            "not_hex",
            "no_dash",
            "plus_version",
            "not_utf8",
            "trailing_slash",
        );
        assert_eq!(count_files("mesh_spec_name"), inputs.len());
        let root = "ab".repeat(32);
        let named = |text: String| text.into_bytes();
        let valid = [
            ("pointer", named(format!("7-{root}"))),
            ("version_0", named(format!("0-{root}"))),
        ];
        for (name, want) in valid {
            assert_eq!(inputs[name], want, "{name}");
            let round_trip = round_trip_spec_name(&want);
            assert_eq!(round_trip.as_deref(), Some(&want[..]), "{name}");
        }
        let refused = [
            ("upper_case", named(format!("7-{}", "AB".repeat(32)))),
            ("short_root", named(format!("7-{}", "ab".repeat(31)))),
            ("long_root", named(format!("7-{root}ab"))),
            ("odd_root", named(format!("7-{root}a"))),
            ("not_hex", named(format!("7-{}gg", "ab".repeat(31)))),
            ("no_dash", named(format!("7{root}"))),
            ("plus_version", named(format!("+7-{root}"))),
            ("trailing_slash", named(format!("7-{root}/"))),
            (
                "not_utf8",
                [&named(format!("7-{root}"))[..], &[0xFF]].concat(),
            ),
        ];
        for (name, want) in refused {
            assert_eq!(inputs[name], want, "{name}");
            assert_eq!(round_trip_spec_name(&want), None, "{name}");
        }
    }
}
