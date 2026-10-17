//! Reads the durable entries of one path back from the ring file, record by
//! record, from the runs of the path's log.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use std::mem;
use std::ops::Range;

use block::{Block, Pool, Unique};
use env::files::File;
use types::channel;
use types::frame::Path;
use types::time::Stamp;

use crate::buffer::Error;
use crate::entry::{self, Header, Samples};
use crate::log::{Mark, Run};
use crate::record::{self, ALIGN, AREA_START, HEADER_LEN};
use crate::wal::Layout;

/// One entry that a read gives back.
#[derive(Clone, Debug)]
pub struct Stored {
    /// The first seq.
    pub first: u64,
    /// The samples, `None` for a caller record.
    pub samples: Option<Samples>,
    /// Mesh time at which the home stored it.
    pub stored_at: Stamp,
    /// The tag the caller gave.
    pub tag: u8,
    /// The bytes of the entry: its parts, joined.
    pub bytes: Block,
}

impl Stored {
    /// The seqs of the samples, from `first`: none for a caller record.
    ///
    /// # Panics
    ///
    /// When the samples end past `u64::MAX`, which no read gives.
    #[must_use]
    pub fn seqs(&self) -> Range<u64> {
        let len = self.samples.map_or(0, |samples| samples.len.get());
        let end = self.first.checked_add(u64::from(len));
        self.first..end.expect("the samples end past u64::MAX")
    }
}

impl PartialEq for Stored {
    fn eq(&self, other: &Self) -> bool {
        self.first == other.first
            && self.samples == other.samples
            && self.stored_at == other.stored_at
            && self.tag == other.tag
            && *self.bytes == *other.bytes
    }
}

impl Eq for Stored {}

/// What one [`Reader::read`](crate::Reader::read) gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    /// The seqs right before the first entry that the path no longer has, as one
    /// range: trimmed, or skipped by an append. When the path holds no entry, the
    /// range ends at its durable tail. Its length is the count of samples lost.
    pub gap: Option<Range<u64>>,
    /// The entries, in order, with no seq left out between them.
    pub entries: Vec<Stored>,
    /// Where the next read continues.
    pub next: Mark,
}

/// One read of a path in progress.
#[derive(Debug)]
pub(crate) struct Reading<'a> {
    file: &'a File,
    pool: &'a Pool,
    layout: Layout,
    path: Path,
    budget: usize,
    /// Pool bytes that the blocks of the entries given take.
    spent: usize,
    read: Read,
    /// The entries of one record to give, with their body offsets and the marks
    /// after them. The read drops the record's table before it takes an entry's
    /// block, so an entry of the pool's largest block reads.
    wanted: Vec<(Header, usize, Mark)>,
}

impl<'a> Reading<'a> {
    /// A read of `path` in the ring `file` from `from`, until the blocks of the
    /// entries given take `budget` pool bytes.
    pub(crate) fn new(
        file: &'a File,
        pool: &'a Pool,
        layout: Layout,
        path: Path,
        from: Mark,
        budget: usize,
    ) -> Self {
        Self {
            file,
            pool,
            layout,
            path,
            budget,
            spent: 0,
            read: Read {
                gap: None,
                entries: Vec::new(),
                next: from,
            },
            wanted: Vec::new(),
        }
    }

    /// Where the read goes on, or `None` once it spent its budget.
    pub(crate) fn next(&self) -> Option<Mark> {
        (self.spent < self.budget).then_some(self.read.next)
    }

    /// What the read gave.
    pub(crate) fn finish(self) -> Read {
        self.read
    }

    /// Tells the read that no record left holds an entry past its mark, and that
    /// the path's durable entries end at `end`. A read with seqs before `end`
    /// reports them as its gap and goes on at `end`.
    ///
    /// # Panics
    ///
    /// When the read holds entries and seqs come before `end`: the hold of a read
    /// keeps a trim from hiding the records after its entries.
    pub(crate) fn end(&mut self, end: Mark) {
        if end.seq > self.read.next.seq {
            assert!(
                self.read.entries.is_empty(),
                "invariant: a read's hold keeps a trim from hiding the records after \
                 its entries"
            );
            self.read.gap = Some(self.read.next.seq..end.seq);
            self.read.next = end;
        }
    }

    /// Gives the entries of `index` on the path in the record of `run` that come
    /// after the read's mark. Returns whether the read goes on to the next record.
    /// A read that holds entries ends at a pool shortage, as at its budget.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a ring read fails, and [`Error::Pool`] when the pool
    /// has no block and the read holds no entry.
    pub(crate) async fn record(
        &mut self,
        index: channel::Key,
        run: Run,
    ) -> Result<bool, Error> {
        match self.entries(index, run).await {
            Err(Error::Pool(_)) if !self.read.entries.is_empty() => Ok(false),
            done => done,
        }
    }

    async fn entries(&mut self, index: channel::Key, run: Run) -> Result<bool, Error> {
        let place = AREA_START + self.layout.place(run.offset);
        let mut wanted = mem::take(&mut self.wanted);
        self.scan(place, index, run, &mut wanted).await?;
        for (header, offset, after) in wanted.drain(..) {
            if self.spent >= self.budget {
                return Ok(false);
            }
            if header.first > self.read.next.seq {
                if !self.read.entries.is_empty() {
                    return Ok(false);
                }
                self.read.gap = Some(self.read.next.seq..header.first);
            }
            let bytes =
                bytes(self.file, self.pool, place, offset, header.bytes).await?;
            self.spent += block::footprint(bytes.len());
            self.read.entries.push(stored(header, bytes));
            self.read.next = after;
        }
        self.wanted = wanted;
        Ok(true)
    }

    /// Puts in `wanted` the entries of `index` on the path in the record of `run`
    /// at `place` that end after the read's mark.
    async fn scan(
        &self,
        place: u64,
        index: channel::Key,
        run: Run,
        wanted: &mut Vec<(Header, usize, Mark)>,
    ) -> Result<(), Error> {
        let table = table(self.file, self.pool, place).await?;
        let mut at = run.start;
        for header in headers(&table) {
            let (header, offset) = header.expect("invariant: a run names a record");
            if (header.index, header.path) != (index, self.path) {
                continue;
            }
            at = at.after(header.first, header.len());
            if at > self.read.next {
                wanted.push((header, offset, at));
            }
        }
        Ok(())
    }
}

/// The stored entry of `header`, with the entry's `bytes`.
pub(crate) fn stored(header: Header, bytes: Block) -> Stored {
    Stored {
        first: header.first,
        samples: header.samples(),
        stored_at: header.stored_at,
        tag: header.tag,
        bytes,
    }
}

/// Each entry header of the record whose header and entry table are `table`, with
/// the body offset of its bytes. Each caller panics on an invalid header: a `map`
/// that panics here costs a read about 1 ns for each header.
///
/// # Panics
///
/// When `table` is not the table of a record that a run names.
fn headers(table: &[u8]) -> entry::Headers<'_> {
    let head = record::head(table).expect("invariant: a run names a record");
    let body = table
        .get(HEADER_LEN..)
        .expect("invariant: the table holds the record header");
    let start = body.get(..head.len).unwrap_or(body);
    entry::parse(start, head.len).expect("invariant: a run names a record")
}

/// The header and entry table of the record at `place` in the ring `file`.
async fn table(file: &File, pool: &Pool, place: u64) -> Result<Unique, Error> {
    let block = file.read_at(place, pool.alloc(ALIGN)?).await?;
    let body = block
        .get(HEADER_LEN..)
        .expect("invariant: a block holds a record header");
    let len =
        HEADER_LEN + entry::table_end(body).expect("invariant: a run names a record");
    if len <= block.len() {
        return Ok(block);
    }
    drop(block);
    Ok(file.read_at(place, pool.alloc(len)?).await?)
}

/// The `len` bytes at body offset `offset` of the record at `place` in the ring
/// `file`. No bytes make no file read.
pub(crate) async fn bytes(
    file: &File,
    pool: &Pool,
    place: u64,
    offset: usize,
    len: u32,
) -> Result<Block, Error> {
    let len = usize::try_from(len).expect("invariant: a u32 fits usize");
    let block = pool.alloc(len)?;
    if len == 0 {
        return Ok(block.freeze());
    }
    let at = u64::try_from(HEADER_LEN + offset).expect("invariant: a body fits u64");
    Ok(file.read_at(place + at, block).await?.freeze())
}

#[cfg(test)]
mod tests {
    use block::{Block, Config, Heap, Pool};
    use types::time::Stamp;

    use std::num::NonZeroU32;

    use super::{Samples, Stored};

    fn block(pool: &Pool, bytes: &[u8]) -> Block {
        pool.copy(bytes).expect("the pool has room")
    }

    fn stored(pool: &Pool, first: u64, samples: Option<Samples>) -> Stored {
        Stored {
            first,
            samples,
            stored_at: Stamp::from_nanos(0),
            tag: 0,
            bytes: block(pool, &[]),
        }
    }

    fn pool() -> Pool {
        let config = Config::new(1 << 20).expect("the budget fits");
        Pool::new(config.clone(), Heap::new(config.reservation()))
    }

    #[test]
    fn the_seqs_of_an_entry_start_at_its_first_and_count_its_samples() {
        let pool = pool();
        let samples = Samples {
            len: NonZeroU32::new(3).expect("not 0"),
            last: None,
        };
        assert_eq!(stored(&pool, 4, Some(samples)).seqs(), 4..7);
        assert_eq!(stored(&pool, 4, None).seqs(), 4..4);
        assert_eq!(stored(&pool, u64::MAX, None).seqs(), u64::MAX..u64::MAX);
    }

    #[test]
    #[should_panic(expected = "the samples end past u64::MAX")]
    fn the_seqs_of_samples_past_the_last_seq_panic() {
        let samples = Samples {
            len: NonZeroU32::MIN,
            last: None,
        };
        drop(stored(&pool(), u64::MAX, Some(samples)).seqs());
    }

    #[test]
    fn stored_entries_differ_when_any_field_differs() {
        let config = Config::new(1 << 20).expect("the budget fits");
        let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
        let base = Stored {
            first: 1,
            samples: Some(Samples {
                len: NonZeroU32::new(2).expect("not 0"),
                last: Some(Stamp::from_nanos(4)),
            }),
            stored_at: Stamp::from_nanos(3),
            tag: 5,
            bytes: block(&pool, &[6, 7]),
        };
        let same = Stored {
            bytes: block(&pool, &[6, 7]),
            ..base.clone()
        };
        assert_eq!(same, base, "equal bytes in another block");
        let changed = [
            Stored {
                first: 0,
                ..base.clone()
            },
            Stored {
                samples: None,
                ..base.clone()
            },
            Stored {
                samples: base.samples.map(|samples| Samples {
                    last: None,
                    ..samples
                }),
                ..base.clone()
            },
            Stored {
                stored_at: Stamp::from_nanos(0),
                ..base.clone()
            },
            Stored {
                tag: 0,
                ..base.clone()
            },
            Stored {
                bytes: block(&pool, &[6, 8]),
                ..base.clone()
            },
        ];
        for (field, stored) in changed.iter().enumerate() {
            assert_ne!(*stored, base, "field {field}");
        }
    }
}
