//! `wal::Writer` and `log::Logs`, for the bench targets only. Not a stable surface.

use std::num::NonZeroU8;

use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;
use crate::record::{ALIGN, BLOCK};
use crate::wal::{Ends, Layout, Position, Writer};

/// The body lengths of the records of a commit, in turn.
const LENS: [usize; 5] = [8, 8, ALIGN, 8, 2 * ALIGN];

/// A write-ahead ring with no file: the writer of a ring that opened empty.
#[derive(Debug)]
pub struct Ring {
    writer: Writer,
    ends: Vec<Ends>,
    /// The writer does not check chain values, so a counter stands in for them.
    chain: u32,
    next: usize,
}

impl Ring {
    /// An empty ring of `blocks` blocks of 4096 bytes, with bodies of at most two
    /// blocks.
    ///
    /// # Panics
    ///
    /// When `blocks` makes no valid layout.
    #[must_use]
    pub fn new(blocks: u64) -> Self {
        let area = blocks.checked_mul(BLOCK).expect("a valid layout");
        let layout = Layout::new(area, 2 * ALIGN).expect("a valid layout");
        Self {
            writer: Writer::empty(layout, 0),
            ends: Vec::new(),
            chain: 9,
            next: 0,
        }
    }

    /// One commit of `records` records, with bodies of 8 bytes, 8 bytes, one block, 8
    /// bytes, and two blocks in turn: `append` for each, then `trimmed`, `synced` for
    /// each, and `release` to the trimmed tail.
    ///
    /// # Panics
    ///
    /// When the ring has no room for a record.
    pub fn commit(&mut self, records: usize) {
        for _ in 0..records {
            let len = LENS[self.next % LENS.len()];
            self.next += 1;
            let plan = self.writer.append(len).expect("the ring has room");
            let wrap = plan.wrap.map(|_| {
                self.chain = self.chain.wrapping_add(1);
                Position::new(plan.offset, self.chain).expect("aligned")
            });
            self.chain = self.chain.wrapping_add(1);
            let record = Position::new(plan.next, self.chain).expect("aligned");
            self.ends.push(Ends { wrap, record });
        }
        let tail = self.writer.trimmed(None);
        for ends in self.ends.drain(..) {
            self.writer.synced(ends);
        }
        if let Some(tail) = tail {
            self.writer.release(tail.offset());
        }
    }
}

/// The durable logs of a shard, synced as each commit syncs them.
#[derive(Debug)]
pub struct Logs {
    inner: crate::log::Logs,
    headers: Vec<(Slot, Header)>,
    offset: u64,
}

impl Logs {
    /// The logs of `indexes` indexes on the live path. With `tag`, each log first
    /// holds a durable entry with that tag.
    #[must_use]
    pub fn new(indexes: u32, tag: Option<NonZeroU8>) -> Self {
        let mut logs = crate::log::Logs::default();
        let headers: Vec<_> = (0..indexes)
            .map(|n| {
                let header = Header {
                    index: channel::Key::from_u128(u128::from(n) + 1),
                    path: Path::Live,
                    first: 0,
                    len: 1,
                    stored_at: Stamp::from_nanos(1),
                    last: Some(Stamp::from_nanos(1)),
                    tag: 0,
                    bytes: 8,
                };
                (Slot::new(n), header)
            })
            .collect();
        let mut offset = BLOCK;
        if let Some(tag) = tag {
            for (slot, header) in &headers {
                let tagged = Header {
                    len: 0,
                    last: None,
                    tag: tag.get(),
                    ..*header
                };
                logs.sync(*slot, &tagged, offset).expect("entries in order");
            }
            offset += BLOCK;
        }
        Self {
            inner: logs,
            headers,
            offset,
        }
    }

    /// One commit of one record that holds one data entry of each index: `hide` to
    /// the record, then `sync` for each entry.
    pub fn commit(&mut self) {
        self.inner.hide(self.offset);
        for (slot, header) in &mut self.headers {
            self.inner
                .sync(*slot, header, self.offset)
                .expect("entries in order");
            header.first += 1;
        }
        self.offset += BLOCK;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart record and records of 1, 1, 2, 1, 3, 1, and 1 blocks fill 11 of
    /// 12 blocks.
    #[test]
    fn a_ring_of_12_blocks_takes_a_commit_of_7_records() {
        Ring::new(12).commit(7);
    }

    /// The 8th record, of 2 blocks, does not fit.
    #[test]
    #[should_panic(expected = "the ring has room")]
    fn a_ring_of_12_blocks_refuses_a_commit_of_8_records() {
        Ring::new(12).commit(8);
    }

    /// The ring has room for each commit only if each commit releases the space of
    /// the one before.
    #[test]
    fn a_ring_of_64_blocks_commits_past_many_wraps() {
        let mut ring = Ring::new(64);
        for _ in 0..1000 {
            ring.commit(8);
        }
    }
}
