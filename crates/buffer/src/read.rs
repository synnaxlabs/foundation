//! Reads the durable entries of one path back from the ring file, record by
//! record, from the runs of the path's log.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use std::ops::Range;

use block::{Block, Pool, Unique};
use env::files::File;
use types::channel;
use types::frame::Path;
use types::time::Stamp;

use crate::buffer::Error;
use crate::entry;
use crate::log::{Mark, Run};
use crate::record::{self, ALIGN, AREA_START, HEADER_LEN};
use crate::wal::Layout;

/// One entry that a read gives back.
#[derive(Clone, Debug)]
pub struct Stored {
    /// The first seq.
    pub first: u64,
    /// How many samples. A caller record has `len` 0.
    pub len: u32,
    /// Mesh time at which the home stored it.
    pub stored_at: Stamp,
    /// The newest stamp of the entry's samples; none for a caller record.
    pub last: Option<Stamp>,
    /// The tag the caller gave.
    pub tag: u8,
    /// The bytes of the entry: its parts, joined.
    pub bytes: Block,
}

impl PartialEq for Stored {
    fn eq(&self, other: &Self) -> bool {
        self.first == other.first
            && self.len == other.len
            && self.stored_at == other.stored_at
            && self.last == other.last
            && self.tag == other.tag
            && *self.bytes == *other.bytes
    }
}

impl Eq for Stored {}

/// What one [`Buffer::read`](crate::Buffer::read) gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    /// The seqs a skip ahead left out right before the first entry, when the read
    /// started at or in them.
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
    /// Bytes of the entries given so far.
    spent: usize,
    read: Read,
}

impl<'a> Reading<'a> {
    /// A read of `path` in the ring `file` from `from`, until an entry would pass
    /// `budget` bytes of entries in all.
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
        let table = self.table(place).await?;
        let head = record::head(&table).expect("invariant: a run names a record");
        let body = table
            .get(HEADER_LEN..)
            .expect("invariant: the table holds the record header");
        let start = body.get(..head.len).unwrap_or(body);
        let headers =
            entry::parse(start, head.len).expect("invariant: a run names a record");
        let mut at = run.start;
        for header in headers {
            let (header, offset) = header.expect("invariant: a run names a record");
            if (header.index, header.path) != (index, self.path) {
                continue;
            }
            let after = at.after(header.first, header.len);
            at = after;
            if after <= self.read.next {
                continue;
            }
            if self.spent >= self.budget {
                return Ok(false);
            }
            if header.first > self.read.next.seq {
                if !self.read.entries.is_empty() {
                    return Ok(false);
                }
                self.read.gap = Some(self.read.next.seq..header.first);
            }
            let bytes = self.bytes(place, offset, header.bytes).await?;
            self.spent += bytes.len();
            self.read.entries.push(Stored {
                first: header.first,
                len: header.len,
                stored_at: header.stored_at,
                last: header.last,
                tag: header.tag,
                bytes,
            });
            self.read.next = after;
        }
        Ok(true)
    }

    /// The header and entry table of the record at `place` in the ring file.
    async fn table(&self, place: u64) -> Result<Unique, Error> {
        let block = self.file.read_at(place, self.pool.alloc(ALIGN)?).await?;
        let body = block
            .get(HEADER_LEN..)
            .expect("invariant: a block holds a record header");
        let len = HEADER_LEN
            + entry::table_end(body).expect("invariant: a run names a record");
        if len <= block.len() {
            return Ok(block);
        }
        drop(block);
        Ok(self.file.read_at(place, self.pool.alloc(len)?).await?)
    }

    /// The `len` bytes at body offset `offset` of the record at `place` in the
    /// ring file. No bytes make no file read.
    async fn bytes(&self, place: u64, offset: usize, len: u32) -> Result<Block, Error> {
        let len = usize::try_from(len).expect("invariant: a u32 fits usize");
        let block = self.pool.alloc(len)?;
        if len == 0 {
            return Ok(block.freeze());
        }
        let at =
            u64::try_from(HEADER_LEN + offset).expect("invariant: a body fits u64");
        Ok(self.file.read_at(place + at, block).await?.freeze())
    }
}

#[cfg(test)]
mod tests {
    use block::{Block, Config, Heap, Pool};
    use types::time::Stamp;

    use super::Stored;

    fn block(pool: &Pool, bytes: &[u8]) -> Block {
        let mut unique = pool.alloc(bytes.len()).expect("the pool has room");
        unique.copy_from_slice(bytes);
        unique.freeze()
    }

    #[test]
    fn stored_entries_differ_when_any_field_differs() {
        let config = Config { budget: 1 << 20 };
        let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
        let base = Stored {
            first: 1,
            len: 2,
            stored_at: Stamp::from_nanos(3),
            last: Some(Stamp::from_nanos(4)),
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
                len: 0,
                ..base.clone()
            },
            Stored {
                stored_at: Stamp::from_nanos(0),
                ..base.clone()
            },
            Stored {
                last: None,
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
