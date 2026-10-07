//! The byte form of a `raft` entry, which the log and the messages share.
//!
//! An entry has one byte form: [`decode`] takes only what [`encode`] gives. A change
//! is signed before it is encoded, so [`decode`] gives each change its signature.

use raft::{Change, Data, Entry, Voters};

use crate::bytes::{
    put_keys, put_position, put_proof, put_signature, take, take_keys, take_position,
    take_proof, take_signature,
};

const EMPTY: u8 = 0;
const BYTES: u8 = 1;
const VOTERS: u8 = 2;

/// Adds the byte form of `entry` to `out`.
///
/// # Panics
///
/// When a change has no signature, as [`put_signature`].
pub(crate) fn encode(entry: &Entry, out: &mut Vec<u8>) {
    put_position(entry.at, out);
    match &entry.data {
        Data::Empty => out.push(EMPTY),
        Data::Bytes(bytes) => {
            out.push(BYTES);
            out.extend(wide(bytes.len()).to_le_bytes());
            out.extend(bytes);
        }
        Data::Voters(change) => {
            out.push(VOTERS);
            put_keys(&change.voters.incoming, out);
            put_keys(&change.voters.outgoing, out);
            put_proof(&change.votes, out);
            put_signature(change.signature, out);
        }
    }
}

/// Takes one entry from the start of `bytes`. `None` when the bytes do not start
/// with the byte form of an entry; `bytes` is then at no known place.
pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Entry> {
    let at = take_position(bytes)?;
    let data = match u8::from_le_bytes(take(bytes)?) {
        EMPTY => Data::Empty,
        BYTES => {
            let len = usize::try_from(u64::from_le_bytes(take(bytes)?)).ok()?;
            let (data, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            Data::Bytes(data.to_vec())
        }
        VOTERS => {
            let voters = Voters {
                incoming: take_keys(bytes)?,
                outgoing: take_keys(bytes)?,
            };
            let proof = take_proof(bytes)?;
            let signature = Some(take_signature(bytes)?);
            Data::Voters(Change {
                voters,
                votes: proof,
                signature,
            })
        }
        _ => return None,
    };
    Some(Entry { at, data })
}

fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}

#[cfg(test)]
mod tests {
    use raft::{Grant, Position, Proof, Signature, Term};

    use super::*;
    use crate::common::key;

    fn change(signature: Option<Signature>) -> Entry {
        let voters = Voters {
            incoming: [key(1), key(2)].into(),
            outgoing: [key(1)].into(),
        };
        let proof = Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [(key(1), Some(Signature([7; 64])))].into(),
        };
        Entry {
            at: Position {
                term: Term(3),
                index: 4,
            },
            data: Data::Voters(Change {
                voters,
                votes: proof,
                signature,
            }),
        }
    }

    #[test]
    fn a_change_is_the_position_tag_voters_votes_and_signature() {
        let mut expected = vec![3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 2];
        expected.extend([2, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.push(1);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([7; 64]);
        expected.extend([9; 64]);
        let entry = change(Some(Signature([9; 64])));
        let mut bytes = Vec::new();
        encode(&entry, &mut bytes);
        assert_eq!(bytes, expected);
        let mut rest = &bytes[..];
        assert_eq!(decode(&mut rest), Some(entry));
        assert!(rest.is_empty());
    }

    #[test]
    fn decode_refuses_a_change_cut_before_its_signature() {
        let mut bytes = Vec::new();
        encode(&change(Some(Signature([9; 64]))), &mut bytes);
        for cut in [bytes.len() - 64, bytes.len() - 1] {
            assert_eq!(decode(&mut &bytes[..cut]), None, "{cut}");
        }
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_change() {
        encode(&change(None), &mut Vec::new());
    }
}
