//! The byte form of a `raft` entry, which the log and the messages share.
//!
//! An entry has one byte form: [`decode`] takes only what [`encode`] gives.

use std::collections::BTreeSet;

use raft::{Data, Entry, Position, Term, Voters};
use types::node;

const EMPTY: u8 = 0;
const BYTES: u8 = 1;
const VOTERS: u8 = 2;

/// Adds the byte form of `entry` to `out`.
pub(crate) fn encode(entry: &Entry, out: &mut Vec<u8>) {
    out.extend(entry.at.term.0.to_le_bytes());
    out.extend(entry.at.index.to_le_bytes());
    match &entry.data {
        Data::Empty => out.push(EMPTY),
        Data::Bytes(bytes) => {
            out.push(BYTES);
            out.extend(wide(bytes.len()).to_le_bytes());
            out.extend(bytes);
        }
        Data::Voters(voters) => {
            out.push(VOTERS);
            for keys in [&voters.incoming, &voters.outgoing] {
                out.extend(wide(keys.len()).to_le_bytes());
                out.extend(keys.iter().flat_map(|key| key.as_u128().to_le_bytes()));
            }
        }
    }
}

/// Takes one entry from the start of `bytes`. `None` when the bytes do not start
/// with the byte form of an entry; `bytes` is then at no known place.
pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Entry> {
    let term = Term(u64::from_le_bytes(take(bytes)?));
    let index = u64::from_le_bytes(take(bytes)?);
    let data = match u8::from_le_bytes(take(bytes)?) {
        EMPTY => Data::Empty,
        BYTES => {
            let len = usize::try_from(u64::from_le_bytes(take(bytes)?)).ok()?;
            let (data, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            Data::Bytes(data.to_vec())
        }
        VOTERS => Data::Voters(Voters {
            incoming: keys(bytes)?,
            outgoing: keys(bytes)?,
        }),
        _ => return None,
    };
    let at = Position { term, index };
    Some(Entry { at, data })
}

/// Takes `N` bytes from the start of `bytes`.
pub(crate) fn take<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = bytes.split_first_chunk()?;
    *bytes = rest;
    Some(*head)
}

/// Takes a node key from the start of `bytes`.
pub(crate) fn key(bytes: &mut &[u8]) -> Option<node::Key> {
    take(bytes).map(|key| node::Key::from_u128(u128::from_le_bytes(key)))
}

fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}

// A count, then the keys in rising order.
fn keys(bytes: &mut &[u8]) -> Option<BTreeSet<node::Key>> {
    let count = u64::from_le_bytes(take(bytes)?);
    let mut keys = BTreeSet::new();
    for _ in 0..count {
        let key = key(bytes)?;
        if keys.last().is_some_and(|last| *last >= key) {
            return None;
        }
        keys.insert(key);
    }
    Some(keys)
}
