//! The deadlines of a commit task with no file, for [`crate::bench`].

use std::num::NonZeroU32;

use block::{Block, Heap, Pool};
use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use super::{State, take};
use crate::entry::{Entry, Samples};
use crate::group::{self, Sealed};
use crate::header::Header;
use crate::log::Logs;
use crate::record::{ALIGN, BLOCK};
use crate::wal::{Layout, Writer};

/// The bytes of the one part of each data entry.
const PART: usize = 100;

/// The commit task of a buffer with no file, over a ring that opened empty.
#[derive(Debug)]
pub struct Deadline {
    state: State,
    pool: Pool,
    block_max: usize,
    part: Block,
    tag: u8,
    /// The next seq of each index.
    firsts: Vec<u64>,
    batch: Vec<Entry>,
    sealed: Vec<Sealed>,
    deadlines: u32,
}

impl Deadline {
    /// The task of a ring of `blocks` blocks of 4096 bytes, with bodies of at most two
    /// blocks, over `indexes` indexes on the live path, and a pool of 2 MiB, as
    /// [`Group`](crate::bench::Group) has.
    ///
    /// # Panics
    ///
    /// When `blocks` makes no valid layout, or `indexes` is over 54, so that a group
    /// of one entry of each index is over a body of two blocks.
    #[must_use]
    pub fn new(indexes: u32, blocks: u64) -> Self {
        let area = blocks.checked_mul(BLOCK).expect("a valid layout");
        let layout = Layout::new(area, 2 * ALIGN).expect("a valid layout");
        let count = usize::try_from(indexes).expect("fits");
        layout
            .check(count, count, count * PART)
            .expect("a group of one entry of each index fits a body of two blocks");
        let config = block::Config::new(1 << 21).expect("the budget fits");
        let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
        let block_max = group::block_max(&pool).expect("the budget holds an entry");
        let part = pool.alloc(PART).expect("the pool has room").freeze();
        let chain = 9;
        let state = State::new(
            Writer::empty(layout, 0),
            Logs::new(area),
            Header::new(layout, chain),
            chain,
        );
        Self {
            state,
            pool,
            block_max,
            part,
            tag: 0,
            firsts: vec![0; count],
            batch: Vec::new(),
            sealed: Vec::new(),
            deadlines: 0,
        }
    }

    /// As [`new`](Self::new), with each entry a state entry of tag 1, whose bytes
    /// each sync copies.
    ///
    /// # Panics
    ///
    /// As [`new`](Self::new) does.
    #[must_use]
    pub fn tagged(indexes: u32, blocks: u64) -> Self {
        Self {
            tag: 1,
            ..Self::new(indexes, blocks)
        }
    }

    /// One deadline after the append of one data entry of 100 bytes of each index
    /// but the last, which appends one each `period` deadlines: the take of the
    /// group, the checkpoint of the trim and the carry record, the seal of the
    /// records, and their sync, with no file write. Gives the tails of the carry
    /// record.
    ///
    /// # Panics
    ///
    /// When the ring or the pool has no room for the group.
    pub fn commit(&mut self, period: u32) -> usize {
        let slow = self.deadlines.is_multiple_of(period);
        self.deadlines = self.deadlines.wrapping_add(1);
        let appended = self.firsts.len().saturating_sub(usize::from(!slow));
        let (part, tag) = (&self.part, self.tag);
        let entries = self.firsts.iter_mut().take(appended).zip(0..);
        self.batch.extend(entries.map(|(first, n)| {
            let entry = Entry {
                index: channel::Key::from_u128(n.into()),
                slot: Slot::new(n),
                path: Path::Live,
                first: *first,
                samples: Some(Samples {
                    len: NonZeroU32::MIN,
                    last: Some(Stamp::from_nanos(9)),
                }),
                stored_at: Stamp::from_nanos(7),
                tag,
                parts: part.clone().into(),
            };
            *first += 1;
            entry
        }));
        let state = &mut self.state;
        state
            .open
            .fit(&self.pool, self.block_max, &state.writer, &self.batch)
            .expect("the ring and the pool have room for the group");
        take(&mut state.open, &mut state.logs, &mut self.batch);
        let (checkpoint, carrying) = state.deadline(&self.pool, &mut self.sealed);
        let tails = carrying
            .as_ref()
            .map_or(0, |carrying| carrying.carried.tails().len());
        let tail = checkpoint.map(|checkpoint| checkpoint.tail);
        state.synced(&mut self.sealed, carrying, tail);
        tails
    }

    /// A [`Deadline::new(indexes, 4096 + PHASE)`](Self::new), after the deadlines of
    /// `commit(1)` past two laps and up to the one before a trim: the next deadline
    /// trims a step and writes a checkpoint.
    ///
    /// # Panics
    ///
    /// As [`new`](Self::new) does.
    #[must_use]
    pub fn trimming(indexes: NonZeroU32) -> Self {
        let blocks = 4096 + crate::bench::PHASE;
        let mut deadline = Self::new(indexes.get(), blocks);
        for _ in 0..=2 * blocks {
            deadline.commit(1);
        }
        let mut trimmed = || {
            let tail = deadline.state.writer.tail();
            deadline.commit(1);
            deadline.state.writer.tail() != tail
        };
        let lap = "a full ring trims within one lap";
        (0..blocks).find(|_| trimmed()).expect(lap);
        let period = (0..blocks).position(|_| trimmed()).expect(lap);
        for _ in 0..period {
            deadline.commit(1);
        }
        deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::PHASE;
    use crate::bench::most_between_trims;

    /// After the warm-up of the `wal` bench, the deadlines of 1 and of 32 indexes
    /// trim over more than 10 laps, and each writes a checkpoint only when it trims.
    /// It reads the private `state`: `commit` gives only the tails of the carry
    /// record.
    #[test]
    fn each_sample_of_deadlines_trims_and_checkpoints_each_trim() {
        for (indexes, most) in [(1, 29), (32, 15)] {
            let mut deadline = Deadline::new(indexes, 64 + PHASE);
            for _ in 0..2 * 64 {
                deadline.commit(1);
            }
            let commit = || {
                let (tail, header) =
                    (deadline.state.writer.tail(), deadline.state.header);
                deadline.commit(1);
                let trimmed = deadline.state.writer.tail() != tail;
                let checkpointed = deadline.state.header != header;
                assert_eq!(checkpointed, trimmed, "a checkpoint comes with each trim");
                deadline.state.writer.tail()
            };
            assert_eq!(most_between_trims(11 * 64, commit), most, "{indexes}");
        }
    }

    /// The deadline after `trimming` of 1 and of 32 indexes, as the `deadline_trim`
    /// bench times it, trims a step, carries no tail, and writes a checkpoint. It
    /// reads the private `state`: `commit` gives only the tails of the carry record.
    #[test]
    fn the_deadline_after_trimming_trims_a_step() {
        for (indexes, freed) in [(1, 101 * BLOCK), (32, 102 * BLOCK)] {
            let nonzero = NonZeroU32::new(indexes).expect("a nonzero count");
            let mut deadline = Deadline::trimming(nonzero);
            let (tail, header) = (deadline.state.writer.tail(), deadline.state.header);
            let area = (4096 + PHASE) * BLOCK;
            assert_eq!(deadline.state.writer.layout().area(), area, "{indexes}");
            assert!(deadline.state.writer.head() > 2 * area, "{indexes}");
            assert_eq!(deadline.commit(1), 0, "{indexes}");
            assert_eq!(deadline.state.writer.tail() - tail, freed, "{indexes}");
            assert_ne!(deadline.state.header, header, "{indexes}");
        }
    }

    /// Each deadline of `commit(1)` of 1 and of 32 indexes writes at least a block
    /// over two laps, so the warm-up of `trimming` passes two laps. It reads the
    /// private `state`: `commit` gives only the tails of the carry record.
    #[test]
    fn each_deadline_writes_a_block() {
        for indexes in [1, 32] {
            let blocks = 64 + PHASE;
            let mut deadline = Deadline::new(indexes, blocks);
            for commit in 0..=2 * blocks {
                let head = deadline.state.writer.head();
                deadline.commit(1);
                let wrote = deadline.state.writer.head() - head;
                assert!(
                    wrote >= BLOCK,
                    "{indexes} indexes, commit {commit}: {wrote}"
                );
            }
        }
    }

    /// `tagged` copies the bytes of each index at each sync, and `new` copies none.
    /// It reads the private `state`: `commit` gives only the tails of the carry record.
    #[test]
    fn a_tagged_deadline_copies_each_index() {
        let tag = std::num::NonZeroU8::MIN;
        let copies = |mut deadline: Deadline| {
            deadline.commit(1);
            let mut newest: Vec<_> = deadline
                .state
                .logs
                .newest(Path::Live, tag)
                .map(|(slot, newest)| (slot, newest.bytes.len()))
                .collect();
            newest.sort_unstable();
            newest
        };
        let each = (0..3).map(|n| (Slot::new(n), PART)).collect::<Vec<_>>();
        assert_eq!(copies(Deadline::tagged(3, 64 + PHASE)), each);
        assert_eq!(copies(Deadline::new(3, 64 + PHASE)), []);
    }

    #[test]
    fn a_deadline_of_no_indexes_takes_no_entry() {
        let mut deadline = Deadline::new(0, 64 + PHASE);
        assert_eq!((deadline.commit(2), deadline.commit(2)), (0, 0));
    }

    #[test]
    fn a_task_of_54_indexes_commits() {
        assert_eq!(Deadline::new(54, 64 + PHASE).commit(1), 0);
    }

    #[test]
    #[should_panic(expected = "a group of one entry of each index fits a body of two \
                               blocks: Body { len: 8309, max: 8192 }")]
    fn a_task_of_55_indexes_panics() {
        let _deadline = Deadline::new(55, 64 + PHASE);
    }
}
