//! The byte primitives that the log, the entries, and the messages share.
//!
//! A `put_*` function adds the byte form of a value to the end of a `Vec`. A `take_*`
//! function takes it from the start of a slice, and gives `None` when the slice is
//! too short; the slice is then at no known place.

use raft::{Position, Term};
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
