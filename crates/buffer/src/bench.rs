//! `wal::Writer`, for the bench target only. Not a stable surface.

use crate::record::{ALIGN, BLOCK};
use crate::wal::{Cursor, Ends, Layout, Position, Step, TABLE, Writer};

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
        let tail = Position::new(0, 9).expect("aligned");
        let mut cursor = Cursor::new(layout, tail, TABLE);
        let zeros = vec![0; cursor.window().len];
        assert_eq!(cursor.next(&zeros), Ok(Step::End), "an empty ring");
        let (writer, _) = cursor.writer(0, 9).expect("an empty ring has room");
        Self {
            writer,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_appends_its_records_in_turn() {
        let mut ring = Ring::new(64);
        ring.commit(5);
        assert_eq!(
            ring.writer.head(),
            9 * BLOCK,
            "the restart record, then records of 1, 1, 2, 1, and 3 blocks"
        );
    }

    #[test]
    fn a_ring_of_64_blocks_commits_past_many_wraps() {
        let mut ring = Ring::new(64);
        for _ in 0..1000 {
            ring.commit(8);
        }
        assert!(
            ring.writer.head() > 12_800 * BLOCK,
            "8000 records of 1.6 blocks each, and the skips of the wraps"
        );
    }
}
