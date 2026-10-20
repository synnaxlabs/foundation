//! The state entries of an index: entries of no samples on its live path, each with
//! the tag of its kind, where the newest of a kind replaces the ones before it.

use std::iter;
use std::num::NonZeroU8;

use block::Block;
use buffer::{Buffer, Entry};
use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

/// The head of a state entry: it goes on the live path of the index `index` at
/// `slot`, at seq `first` and mesh time `stored_at`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Head {
    pub(crate) index: channel::Key,
    pub(crate) slot: Slot,
    pub(crate) first: u64,
    pub(crate) stored_at: Stamp,
}

/// Where a state entry goes: `buffer`, with `head`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Target<'a> {
    pub(crate) buffer: &'a Buffer,
    pub(crate) head: Head,
}

/// Appends the state entry of tag `tag` with `body` in a block of the pool of
/// `buffer`, or no part for `None`, with `head`, then `with` in the same record, or
/// alone for `None`. A crash keeps both or neither. Returns whether the entries need
/// no later append: the buffer took them, or a failed commit ended the buffer, so a
/// restart reads no later entry. `false` when the pool had no block for the body, or
/// the ring or the pool had no room.
///
/// # Panics
///
/// If the body passes the most bytes of a block or of one entry of the ring. Each
/// kind keeps its bodies within them.
pub(crate) fn append(
    buffer: &Buffer,
    tag: NonZeroU8,
    body: Option<&[u8]>,
    head: Head,
    with: Option<Entry>,
) -> bool {
    let block = body.map(|body| {
        let mut block = buffer.pool().alloc(body.len())?;
        block.copy_from_slice(body);
        Ok(block.freeze())
    });
    let body = match block.transpose() {
        Ok(body) => body,
        Err(block::Error::Exhausted { .. } | block::Error::Refused { .. }) => {
            return false;
        }
        Err(error @ block::Error::TooLarge { .. }) => {
            panic!("invariant: a block holds the body of a state entry: {error}")
        }
    };
    match buffer.append(iter::once(entry(tag, body, head)).chain(with)) {
        Ok(()) | Err(buffer::Rejected::Files(_)) => true,
        Err(buffer::Rejected::Full { .. } | buffer::Rejected::Pool(_)) => false,
        Err(buffer::Rejected::Large(limit)) => {
            panic!(
                "invariant: one record holds a state entry and its companion: {limit}"
            )
        }
    }
}

/// The state entry of tag `tag` with `body`, or no part for `None`, with `head`.
fn entry(tag: NonZeroU8, body: Option<Block>, head: Head) -> Entry {
    Entry {
        index: head.index,
        slot: head.slot,
        path: Path::Live,
        first: head.first,
        samples: None,
        stored_at: head.stored_at,
        tag: tag.get(),
        parts: body.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{create_pool, key};

    fn head() -> Head {
        Head {
            index: key(Slot::new(4)),
            slot: Slot::new(4),
            first: 12,
            stored_at: Stamp::from_nanos(9),
        }
    }

    #[test]
    fn puts_the_entry_on_the_live_path_at_first() {
        let mut block = create_pool(4096).alloc(3).expect("room");
        block.copy_from_slice(b"abc");

        let entry = entry(NonZeroU8::MIN, Some(block.freeze()), head());

        let place = (entry.index, entry.slot, entry.path, entry.first);
        assert_eq!(place, (key(Slot::new(4)), Slot::new(4), Path::Live, 12));
        assert_eq!(
            (entry.samples, entry.stored_at, entry.tag),
            (None, Stamp::from_nanos(9), 1)
        );
        let parts: Vec<_> = entry.parts.into_iter().map(|part| part.to_vec()).collect();
        assert_eq!(parts, [b"abc".to_vec()]);
    }

    #[test]
    fn has_no_part_for_no_body() {
        let entry = entry(NonZeroU8::MIN, None, head());

        assert_eq!(entry.parts.into_iter().count(), 0);
    }
}
