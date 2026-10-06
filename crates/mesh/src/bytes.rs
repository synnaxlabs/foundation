//! The byte primitives that the log, the entries, and the messages share.
//!
//! A `put_*` function adds the byte form of a value to the end of a `Vec`. A `take_*`
//! function takes it from the start of a slice, and gives `None` when the slice is
//! too short; the slice is then at no known place.

use std::collections::{BTreeMap, BTreeSet};

use raft::{Grant, Position, Proof, Signature, Term};
use types::node;

/// Takes `N` bytes.
pub(crate) fn take<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = bytes.split_first_chunk()?;
    *bytes = rest;
    Some(*head)
}

/// Adds a node key as 16 little-endian bytes.
pub(crate) fn put_key(key: node::Key, out: &mut Vec<u8>) {
    out.extend(key.as_u128().to_le_bytes());
}

/// Takes a node key.
pub(crate) fn take_key(bytes: &mut &[u8]) -> Option<node::Key> {
    take(bytes).map(|key| node::Key::from_u128(u128::from_le_bytes(key)))
}

/// Adds a position as its term, then its index.
pub(crate) fn put_position(at: Position, out: &mut Vec<u8>) {
    out.extend(at.term.0.to_le_bytes());
    out.extend(at.index.to_le_bytes());
}

/// Takes a position.
pub(crate) fn take_position(bytes: &mut &[u8]) -> Option<Position> {
    let term = Term(u64::from_le_bytes(take(bytes)?));
    let index = u64::from_le_bytes(take(bytes)?);
    Some(Position { term, index })
}

/// Adds a count as 8 little-endian bytes, then the keys in rising order.
pub(crate) fn put_keys(keys: &BTreeSet<node::Key>, out: &mut Vec<u8>) {
    let count = u64::try_from(keys.len()).expect("invariant: a count fits in 64 bits");
    out.extend(count.to_le_bytes());
    for &key in keys {
        put_key(key, out);
    }
}

/// Takes what [`put_keys`] gives. `None` when the keys are not in rising order.
pub(crate) fn take_keys(bytes: &mut &[u8]) -> Option<BTreeSet<node::Key>> {
    let count = u64::from_le_bytes(take(bytes)?);
    let mut keys = BTreeSet::new();
    for _ in 0..count {
        let key = take_key(bytes)?;
        if keys.last().is_some_and(|last| *last >= key) {
            return None;
        }
        keys.insert(key);
    }
    Some(keys)
}

pub(crate) const ABSENT: u8 = 0;
pub(crate) const PRESENT: u8 = 1;
pub(crate) const PRE_VOTE: u8 = 0;
pub(crate) const VOTE: u8 = 1;

/// Adds a presence byte, then the key when there is one.
pub(crate) fn put_optional_key(key: Option<node::Key>, out: &mut Vec<u8>) {
    match key {
        None => out.push(ABSENT),
        Some(key) => {
            out.push(PRESENT);
            put_key(key, out);
        }
    }
}

/// Adds a signature's 64 bytes.
///
/// # Panics
///
/// When `signature` is `None`: the caller signs every grant before it encodes one.
pub(crate) fn put_signature(signature: Option<Signature>, out: &mut Vec<u8>) {
    let Signature(bytes) =
        signature.expect("invariant: a grant is signed before it is encoded");
    out.extend(bytes);
}

/// Takes a signature.
pub(crate) fn take_signature(bytes: &mut &[u8]) -> Option<Signature> {
    take(bytes).map(Signature)
}

/// Adds a presence byte, then the proof when there is one: its grant as one byte,
/// the candidate, a count of voters as 8 little-endian bytes, then each voter's key
/// and signature in rising key order.
///
/// # Panics
///
/// When a voter has no signature, as [`put_signature`].
pub(crate) fn put_optional_proof(proof: Option<&Proof>, out: &mut Vec<u8>) {
    match proof {
        None => out.push(ABSENT),
        Some(proof) => {
            out.push(PRESENT);
            out.push(match proof.grant {
                Grant::PreVote => PRE_VOTE,
                Grant::Vote => VOTE,
            });
            put_key(proof.candidate, out);
            let count = u64::try_from(proof.voters.len())
                .expect("invariant: a count fits in 64 bits");
            out.extend(count.to_le_bytes());
            for (&voter, &signature) in &proof.voters {
                put_key(voter, out);
                put_signature(signature, out);
            }
        }
    }
}

/// Takes a presence byte. `None` for a byte that is neither value.
pub(crate) fn take_present(bytes: &mut &[u8]) -> Option<bool> {
    match u8::from_le_bytes(take(bytes)?) {
        ABSENT => Some(false),
        PRESENT => Some(true),
        _ => None,
    }
}

/// Takes what [`put_optional_proof`] gives after its presence byte. `None` when the
/// voters are not in rising order.
pub(crate) fn take_proof(bytes: &mut &[u8]) -> Option<Proof> {
    let grant = match u8::from_le_bytes(take(bytes)?) {
        PRE_VOTE => Grant::PreVote,
        VOTE => Grant::Vote,
        _ => return None,
    };
    let candidate = take_key(bytes)?;
    let count = u64::from_le_bytes(take(bytes)?);
    let mut voters = BTreeMap::new();
    for _ in 0..count {
        let voter = take_key(bytes)?;
        if voters
            .last_key_value()
            .is_some_and(|(last, _)| *last >= voter)
        {
            return None;
        }
        voters.insert(voter, Some(take_signature(bytes)?));
    }
    Some(Proof {
        grant,
        candidate,
        voters,
    })
}
