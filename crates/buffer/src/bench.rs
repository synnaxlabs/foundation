//! `wal::Writer` and `log::Logs`, for the bench targets and the `alloc` test only. Not
//! a stable surface.

use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;
use crate::log::Carried;
use crate::record::{ALIGN, BLOCK};
use crate::wal::{Ends, Layout, Position, Writer};

/// The area of the ring of [`Logs`]: 256 records of a block.
const AREA: u64 = 256 * BLOCK;

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
    /// bytes, and two blocks in turn: `append` for each, then `trimmed`, and
    /// `committed` with the records and the trimmed tail.
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
        let tail = self.writer.trimmed(|offset| offset);
        self.writer.committed(self.ends.drain(..), tail);
    }
}

/// The durable logs of a shard, synced as each commit syncs them.
#[derive(Debug)]
pub struct Logs {
    inner: crate::log::Logs,
    headers: Vec<(Slot, Header)>,
    offset: u64,
    commits: u32,
    tails: Carried,
}

impl Logs {
    /// The logs of `indexes` indexes on the live path.
    #[must_use]
    pub fn new(indexes: u32) -> Self {
        Self::of(indexes, 1, Some(Stamp::from_nanos(1)))
    }

    /// The logs of `indexes` indexes on the live path, whose entries have no
    /// samples and come at the seq of the entry before. Each sync in a later
    /// record marks a range.
    #[must_use]
    pub fn empty(indexes: u32) -> Self {
        Self::of(indexes, 0, None)
    }

    /// The logs of `indexes` indexes whose entries hold `len` samples up to `last`.
    fn of(indexes: u32, len: u32, last: Option<Stamp>) -> Self {
        let headers = (0..indexes)
            .map(|n| {
                let header = Header {
                    index: channel::Key::from_u128(u128::from(n) + 1),
                    path: Path::Live,
                    first: 0,
                    len,
                    stored_at: Stamp::from_nanos(1),
                    last,
                    tag: 0,
                    bytes: 8,
                };
                (Slot::new(n), header)
            })
            .collect();
        Self {
            inner: crate::log::Logs::new(AREA),
            headers,
            offset: BLOCK,
            commits: 0,
            tails: Carried::default(),
        }
    }

    /// One commit of one record that holds `entries` data entries of each index. It
    /// first hides the records before the oldest holder, as a trim that keeps up, so
    /// each log holds the run of its holder. Then it syncs each entry.
    pub fn commit(&mut self, entries: u32) {
        self.keep_up();
        for (slot, header) in &mut self.headers {
            for _ in 0..entries {
                self.inner
                    .sync(*slot, header, self.offset)
                    .expect("entries in order");
                header.first += u64::from(header.len);
            }
        }
        self.offset += BLOCK;
    }

    /// One commit as `commit(1)`, with an append of each entry, then a carry record
    /// in the next record that holds the durable tail of each index, so the hide of
    /// the next commit drops the run of each index.
    pub fn carry(&mut self) {
        self.keep_up();
        for (slot, header) in &mut self.headers {
            self.inner.append(*slot, header).expect("entries in order");
            self.inner
                .sync(*slot, header, self.offset)
                .expect("entries in order");
            header.first += u64::from(header.len);
        }
        self.offset += BLOCK;
        for (slot, header) in &self.headers {
            let tail = self.inner.end(*slot, header.path);
            self.inner
                .carry(*slot, &tail, self.offset)
                .expect("the durable tail");
        }
        self.offset += BLOCK;
    }

    /// Moves the appended tail of each index past one data entry, with no sync.
    pub fn append(&mut self) {
        for (slot, header) in &mut self.headers {
            self.inner.append(*slot, header).expect("entries in order");
            header.first += u64::from(header.len);
        }
    }

    /// One commit that the oldest holder bounds, as a commit that trims. It appends
    /// and syncs one data entry of each index but the last, which syncs one each
    /// `period` commits, so its item is the last of its record. Then it takes the
    /// idle tails before the record, at most one per index, and hides the records
    /// before the landing of the oldest holder, or of the record `held` commits
    /// back, as a read that holds it, when that one is older.
    pub fn trim(&mut self, period: u32, held: u64) {
        let slow = self.commits.is_multiple_of(period);
        self.commits = self.commits.wrapping_add(1);
        let synced = self.headers.len() - usize::from(!slow);
        for (slot, header) in self.headers.iter_mut().take(synced) {
            self.inner.append(*slot, header).expect("entries in order");
            self.inner
                .sync(*slot, header, self.offset)
                .expect("entries in order");
            header.first += u64::from(header.len);
        }
        self.tails.clear();
        self.inner
            .idle(self.offset, self.headers.len(), &mut self.tails);
        let kept = self.inner.kept().expect("a path has a synced entry");
        let read = self.offset.saturating_sub(held * BLOCK).max(BLOCK);
        let landing = self.inner.marked().land(kept.min(read));
        self.inner.hide(landing);
        self.offset += BLOCK;
    }

    /// One commit that marks a range for each index, as a commit that trims and
    /// keeps up. It hides the records before the landing of the oldest holder,
    /// syncs one entry with no samples of each index at the seq of its durable
    /// tail, then carries the durable end of each index in the next record.
    pub fn mark(&mut self) {
        self.keep_up();
        for (slot, header) in &self.headers {
            let empty = Header {
                len: 0,
                last: None,
                ..*header
            };
            self.inner.append(*slot, &empty).expect("entries in order");
            self.inner
                .sync(*slot, &empty, self.offset)
                .expect("entries in order");
        }
        self.offset += BLOCK;
        self.tails.clear();
        self.inner
            .idle(self.offset, self.headers.len(), &mut self.tails);
        let tails = self.tails.tails();
        assert_eq!(tails.len(), self.headers.len(), "a tail of each index");
        for (tail, (_, header)) in tails.iter().zip(&self.headers) {
            assert_eq!(tail.index, header.index, "a tail of each index, in order");
        }
        self.inner.carried(&self.tails, self.offset);
        self.offset += BLOCK;
    }

    /// Hides the records before the landing of the oldest holder, as a trim that
    /// keeps up.
    fn keep_up(&mut self) {
        if let Some(kept) = self.inner.kept() {
            let landing = self.inner.marked().land(kept);
            self.inner.hide(landing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart record and records of 1, 1, 2, 1, 3, 1, and 1 blocks fill 11 of
    /// 14 blocks, and leave a block for the carry record and one for the restart
    /// record.
    #[test]
    fn a_ring_of_14_blocks_takes_a_commit_of_7_records() {
        Ring::new(14).commit(7);
    }

    /// The 8th record, of 2 blocks, does not fit with those two blocks.
    #[test]
    #[should_panic(expected = "the ring has room")]
    fn a_ring_of_14_blocks_refuses_a_commit_of_8_records() {
        Ring::new(14).commit(8);
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

    /// Each commit moves the durable tail of each index past one more entry, and
    /// each log keeps two runs: the run of its holder, and the new one. It reads the
    /// private logs: `commit` gives nothing back, and only the `logs` bench and the
    /// `alloc` and `memory` tests call it, whose counts do not change when a commit
    /// skips the hide.
    #[test]
    fn each_commit_of_the_logs_syncs_one_entry_of_each_index() {
        let mut logs = Logs::new(2);
        for _ in 0..3 {
            logs.commit(2);
        }
        for n in 0..2 {
            let slot = Slot::new(n);
            assert_eq!(logs.inner.durable(slot, Path::Live).seq, 6, "index {n}");
            assert_eq!(logs.inner.runs(slot, Path::Live).count(), 2, "index {n}");
        }
    }

    /// Each carry record becomes the holder of each index, and its tail is the end.
    #[test]
    fn each_carry_of_the_logs_holds_the_tail_of_each_index() {
        let mut logs = Logs::new(2);
        logs.carry();
        logs.carry();
        for n in 0..2 {
            let slot = Slot::new(n);
            assert_eq!(logs.inner.durable(slot, Path::Live).seq, 2, "index {n}");
        }
        assert_eq!(logs.inner.kept(), Some(4 * BLOCK));
    }

    /// On logs whose entries have no samples, each carry record holds the durable
    /// given of each index, which grows past 0.
    #[test]
    fn a_carry_of_logs_with_no_samples_holds_the_tail_of_each_index() {
        let mut logs = Logs::empty(2);
        logs.carry();
        logs.carry();
        for n in 0..2 {
            let tail = logs.inner.end(Slot::new(n), Path::Live);
            assert_eq!((tail.seq, tail.given), (0, 2), "index {n}");
        }
        assert_eq!(logs.inner.kept(), Some(4 * BLOCK));
    }

    /// Each call moves the appended tail of each index past one more entry.
    #[test]
    fn each_append_of_the_logs_moves_each_index() {
        let mut logs = Logs::new(2);
        logs.append();
        logs.append();
        for n in 0..2 {
            let slot = Slot::new(n);
            assert_eq!(logs.inner.appended(slot, Path::Live).seq, 2, "index {n}");
            assert_eq!(logs.inner.durable(slot, Path::Live).seq, 0, "index {n}");
        }
    }

    /// The last index syncs each third commit, so a trim keeps the records from its
    /// holder on, and its tail is idle once for each holder.
    #[test]
    fn a_trim_keeps_the_records_after_the_holder_of_the_slow_index() {
        let mut logs = Logs::new(2);
        let mut idle = Vec::new();
        for _ in 0..7 {
            logs.trim(3, 0);
            idle.push(logs.tails.tails().len());
        }
        assert_eq!(idle, [0, 1, 0, 0, 1, 0, 0]);
        let first: Vec<_> = logs.inner.runs(Slot::new(1), Path::Live).collect();
        let second = logs.inner.runs(Slot::new(0), Path::Live).count();
        assert_eq!(
            first.iter().map(|run| run.offset).collect::<Vec<_>>(),
            [7 * BLOCK]
        );
        assert_eq!(second, 1);
        assert_eq!(logs.inner.durable(Slot::new(1), Path::Live).seq, 3);
    }

    /// A read holds the record two commits back, so the runs of the records after it
    /// stay.
    #[test]
    fn a_trim_keeps_the_records_from_the_one_a_read_holds() {
        let mut logs = Logs::new(2);
        for _ in 0..7 {
            logs.trim(3, 2);
        }
        let runs = logs.inner.runs(Slot::new(0), Path::Live);
        let offsets: Vec<u64> = runs.map(|run| run.offset).collect();
        assert_eq!(offsets, [5 * BLOCK, 6 * BLOCK, 7 * BLOCK]);
    }

    /// Each call after the first marks the range from the carry record of the call
    /// before to its data record, and the trim of the next call passes it, so only
    /// the newest range stays. An entry with no samples moves no stamp. It reads the
    /// private logs: `mark` gives nothing back, and only the `logs` bench and the
    /// `alloc` test call it. So only this test kills the mutant that deletes
    /// `last: None` in `mark`: it changes only the stamp of a bench entry, which no
    /// caller reads.
    #[test]
    fn each_mark_of_the_logs_marks_one_range_and_hides_the_one_before() {
        let mut logs = Logs::new(2);
        for _ in 0..3 {
            logs.mark();
        }
        assert_eq!(logs.inner.marked().land(3 * BLOCK), 3 * BLOCK);
        assert_eq!(logs.inner.marked().land(5 * BLOCK), 4 * BLOCK);
        assert_eq!(logs.inner.kept(), Some(6 * BLOCK));
        for n in 0..2 {
            let runs = logs.inner.runs(Slot::new(n), Path::Live);
            let offsets: Vec<u64> = runs.map(|run| run.offset).collect();
            assert_eq!(offsets, [5 * BLOCK], "index {n}");
            let stamp = logs.inner.durable(Slot::new(n), Path::Live).stamp;
            assert_eq!(stamp, None, "index {n}");
        }
    }
}
