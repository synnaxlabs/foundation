//! The byte form of `raft` entries one after another, which the log and an append
//! share.
//!
//! Entries have one byte form: [`decode`] takes only what [`encode`] gives. A change
//! is signed before it is encoded, so [`decode`] gives each change its signature.

use raft::{Data, Entry};

use crate::bytes::{
    change_len, put_change, put_position, take, take_change, take_position,
};
use crate::change;

const EMPTY: u8 = 0;
const BYTES: u8 = 1;
const VOTERS: u8 = 2;

/// The bytes of the largest entry that an `Append` takes: one that holds a record of
/// [`change::BYTES_MAX`] bytes.
pub(crate) const BYTES_MAX: usize = 16 + 1 + 8 + change::BYTES_MAX;

/// Adds the byte form of `entries` to `out`.
///
/// # Panics
///
/// When a change has no signature, as [`put_change`].
pub(crate) fn encode(entries: &[Entry], out: &mut Vec<u8>) {
    for entry in entries {
        put_one(entry, out);
    }
}

/// The bytes that [`encode`] adds for `entries`, with 64 for each signature, also one
/// not yet made.
pub(crate) fn len(entries: &[Entry]) -> usize {
    let data = |data: &Data| match data {
        Data::Empty => 0,
        Data::Bytes(bytes) => bytes.len().saturating_add(8),
        Data::Voters(change) => change_len(change),
    };
    let lengths = entries
        .iter()
        .map(|entry| data(&entry.data).saturating_add(16 + 1));
    lengths.fold(0, usize::saturating_add)
}

/// The entries that `bytes` is the byte form of, up to its end. `None` when it is not
/// the byte form of entries.
pub(crate) fn decode(mut bytes: &[u8]) -> Option<Vec<Entry>> {
    let mut entries = Vec::new();
    while !bytes.is_empty() {
        entries.push(take_one(&mut bytes)?);
    }
    Some(entries)
}

fn put_one(entry: &Entry, out: &mut Vec<u8>) {
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
            put_change(change, out);
        }
    }
}

// `None` when `bytes` does not start with the byte form of an entry.
fn take_one(bytes: &mut &[u8]) -> Option<Entry> {
    let at = take_position(bytes)?;
    let data = match u8::from_le_bytes(take(bytes)?) {
        EMPTY => Data::Empty,
        BYTES => {
            let len = usize::try_from(u64::from_le_bytes(take(bytes)?)).ok()?;
            let (data, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            Data::Bytes(data.to_vec())
        }
        VOTERS => Data::Voters(take_change(bytes)?),
        _ => return None,
    };
    Some(Entry { at, data })
}

fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}

#[cfg(test)]
mod tests {
    use raft::{Change, Grant, Proof, Signature, Voters};
    use types::ed25519::PublicKey;

    use super::*;
    use crate::common::{at, key};

    // A change that adds nodes 2 and 3 to node 1.
    fn change(signature: Option<Signature>) -> Entry {
        let voters = Voters {
            incoming: [key(1), key(2), key(3)].into(),
            outgoing: [key(1)].into(),
        };
        let public = |byte| PublicKey::new([byte; 32]).unwrap();
        let keys = [(key(2), public(0xb2)), (key(3), public(0xb3))].into();
        let proof = Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: [(key(1), Some(Signature([7; 64])))].into(),
        };
        Entry {
            at: at(3, 4),
            data: Data::Voters(Change {
                voters,
                public_keys: keys,
                votes: proof,
                signature,
            }),
        }
    }

    #[test]
    fn a_change_is_the_position_tag_voters_bound_keys_votes_and_signature() {
        let mut expected = vec![3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 2];
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([0xb2; 32]);
        expected.extend([0xb3; 32]);
        expected.push(1);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([7; 64]);
        expected.extend([9; 64]);
        let entries = vec![change(Some(Signature([9; 64])))];
        let mut bytes = Vec::new();
        encode(&entries, &mut bytes);
        assert_eq!(bytes, expected);
        assert_eq!(decode(&bytes), Some(entries));
    }

    #[test]
    fn decode_refuses_a_change_cut_before_its_signature() {
        let mut bytes = Vec::new();
        encode(&[change(Some(Signature([9; 64])))], &mut bytes);
        for cut in [bytes.len() - 64, bytes.len() - 1] {
            assert_eq!(decode(&bytes[..cut]), None, "{cut}");
        }
    }

    // The byte form of the signed `change`, and where its first bound key starts.
    fn encoded_change() -> (Vec<u8>, usize) {
        let mut bytes = Vec::new();
        encode(&[change(Some(Signature([9; 64])))], &mut bytes);
        (bytes, 17 + 8 + 3 * 16 + 8 + 16)
    }

    #[test]
    fn decode_refuses_a_change_cut_inside_a_bound_key() {
        let (bytes, first) = encoded_change();
        for cut in [first, first + 31, first + 32, first + 63] {
            assert_eq!(decode(&bytes[..cut]), None, "{cut}");
        }
    }

    #[test]
    fn decode_refuses_a_bound_key_of_small_order() {
        let (mut bytes, first) = encoded_change();
        let mut identity = [0; 32];
        identity[0] = 1;
        for at in [first, first + 32] {
            let mut bytes = bytes.clone();
            bytes[at..at + 32].copy_from_slice(&identity);
            assert_eq!(decode(&bytes), None, "{at}");
        }
        bytes[first..first + 32].copy_from_slice(&[0xb4; 32]);
        assert!(decode(&bytes).is_some());
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_change() {
        encode(&[change(None)], &mut Vec::new());
    }
}
