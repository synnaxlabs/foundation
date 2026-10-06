//! The byte form of a `raft` entry, which the log and the messages share.
//!
//! An entry has one byte form: [`decode`] takes only what [`encode`] gives.

use raft::{Data, Entry, Voters};

use crate::bytes::{put_keys, put_position, take, take_keys, take_position};

const EMPTY: u8 = 0;
const BYTES: u8 = 1;
const VOTERS: u8 = 2;

/// Adds the byte form of `entry` to `out`.
pub(crate) fn encode(entry: &Entry, out: &mut Vec<u8>) {
    put_position(entry.at, out);
    match &entry.data {
        Data::Empty => out.push(EMPTY),
        Data::Bytes(bytes) => {
            out.push(BYTES);
            out.extend(wide(bytes.len()).to_le_bytes());
            out.extend(bytes);
        }
        Data::Voters(voters) => {
            out.push(VOTERS);
            put_keys(&voters.incoming, out);
            put_keys(&voters.outgoing, out);
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
        VOTERS => Data::Voters(Voters {
            incoming: take_keys(bytes)?,
            outgoing: take_keys(bytes)?,
        }),
        _ => return None,
    };
    Some(Entry { at, data })
}

fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}
