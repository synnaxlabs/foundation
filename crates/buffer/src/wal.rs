//! Places the records of the write-ahead ring and walks them at recovery. It does no
//! I/O: the caller writes what a [`Writer`] plans and reads what a [`Cursor`] asks.
//!
//! The ring is an area of [`ALIGN`]-byte blocks, used in a circle. An offset counts
//! bytes since the ring was made and never wraps; its place in the area is the
//! offset modulo the area length. A record never crosses the end of the area: when
//! it does not fit in the rest, a wrap record goes there and the record goes to the
//! start. Each open of the ring walks it with a [`Cursor`], which then gives the
//! [`Writer`] and its restart record; the chain continues from the random value in
//! that record. A ring whose head reaches the end of the offsets is full for good.
//!
//! The writer places records by their length and makes no chain value.
//! [`Plan::seal`] makes the headers of a placed record from the chain value before
//! it, so the CRC over the body runs where the caller seals, in record order. The
//! writer keeps the boundary after each synced record, which is where a trim can
//! move the tail.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use std::collections::VecDeque;
use std::fmt;

use crate::entry::{self, ENTRIES_MAX};
use crate::record::{
    self, ALIGN, AREA_START, BLOCK, Body, Check, HEADER_LEN, Head, Kind, Record,
};

/// The body of a restart record: one chain value.
const RESTART_LEN: usize = 4;

/// Bytes of the whole blocks that hold a restart record: one block.
const RESTART: usize = ALIGN;
const _: () = assert!(HEADER_LEN + RESTART_LEN <= RESTART, "a restart record fits");

/// The smallest body a layout allows: the rest of a block after the record header.
/// A record takes whole blocks, so a smaller body saves no disk.
const BODY_MIN: usize = ALIGN - HEADER_LEN;
const _: () = assert!(entry::table_len(1) <= BODY_MIN, "a body holds one entry");

/// Bytes of the whole blocks that hold a record header and the largest entry table.
const TABLE: usize = (HEADER_LEN + entry::TABLE_MAX).next_multiple_of(ALIGN);

fn to_u64(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length in memory fits in u64")
}

/// The size of the largest record of a body of at most `body_max` bytes, in whole
/// blocks, or `None` when `body_max` is under one block less the record header, over
/// `u32::MAX`, or so large that the record does not fit in a `usize`.
fn window(body_max: usize) -> Option<u64> {
    let body = BODY_MIN..=usize::try_from(u32::MAX).unwrap_or(usize::MAX);
    let window = HEADER_LEN
        .checked_add(body_max)?
        .checked_next_multiple_of(ALIGN)?;
    body.contains(&body_max).then(|| to_u64(window))
}

/// The smallest area with a largest record of `window` bytes: the restart record,
/// the skip of less than a record before the end of the area, then the record.
fn area_min(window: u64) -> u64 {
    to_u64(RESTART) + (window - BLOCK) + window
}

/// An offset that is not on a block boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unaligned {
    pub(crate) offset: u64,
}

/// A record boundary: where the next record starts and the chain value it follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    offset: u64,
    chain: u32,
}

impl Position {
    /// A boundary read from the ring header. A boundary after a record comes
    /// from [`Plan::seal`] or [`Writer::trimmed`].
    ///
    /// # Errors
    ///
    /// [`Unaligned`] when `offset` is not a multiple of [`ALIGN`].
    pub(crate) fn new(offset: u64, chain: u32) -> Result<Self, Unaligned> {
        if offset.is_multiple_of(BLOCK) {
            Ok(Self { offset, chain })
        } else {
            Err(Unaligned { offset })
        }
    }

    pub(crate) fn offset(self) -> u64 {
        self.offset
    }

    pub(crate) fn chain(self) -> u32 {
        self.chain
    }
}

/// Sizes that do not make a ring. [`Layout::new`] says which sizes do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unfit {
    /// The area that was asked for, in bytes.
    pub area: u64,
    /// The largest record body that was asked for, in bytes.
    pub body_max: usize,
}

/// A file length that holds no ring. [`Layout::fit`] says which lengths do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Small {
    /// The length that was given, in bytes.
    pub len: u64,
    /// The least length that holds a ring of the `body_max` that was given, in
    /// bytes.
    pub min: u64,
}

impl fmt::Display for Small {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { len, min } = self;
        write!(
            f,
            "a ring file of {len} bytes holds no ring; it needs at least {min} bytes"
        )
    }
}

impl std::error::Error for Small {}

/// The sizes of one ring: the area in bytes and the most bytes one record body
/// holds. A record is one group commit, so `body_max` bounds a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    area: u64,
    body_max: usize,
    /// The size of the largest record.
    window: u64,
}

impl Layout {
    /// The least [`entry_max`](Self::entry_max) of any layout: the bytes of parts
    /// that one entry alone holds in a record at the smallest `body_max`.
    pub const ENTRY_MAX_MIN: usize = BODY_MIN - entry::table_len(1);

    /// A ring of `area` bytes whose records hold a body of at most `body_max` bytes.
    ///
    /// # Errors
    ///
    /// [`Unfit`] when `body_max` is under 4087 bytes (one block less the record
    /// header), over `u32::MAX`, or so large that the largest record does not fit in a
    /// `usize`. Also when `area` is not a multiple of [`ALIGN`], when it is less than
    /// twice the largest record (a 9-byte header and `body_max`, in whole 4096-byte
    /// blocks), or when the ring file (two header blocks and the area) does not fit in
    /// a `u64`. A ring of that length that holds only its restart record takes any
    /// record, wherever the restart record is.
    pub fn new(area: u64, body_max: usize) -> Result<Self, Unfit> {
        match window(body_max) {
            Some(window)
                if area.is_multiple_of(BLOCK)
                    && area <= u64::MAX - AREA_START
                    && area_min(window) <= area =>
            {
                Ok(Self {
                    area,
                    body_max,
                    window,
                })
            }
            _ => Err(Unfit { area, body_max }),
        }
    }

    /// The largest ring whose file takes at most `len` bytes, with records of a
    /// body of at most `body_max` bytes.
    ///
    /// # Errors
    ///
    /// [`Small`] when `len` holds no ring of that `body_max`.
    ///
    /// # Panics
    ///
    /// When `body_max` is a size that [`Layout::new`] refuses at any area: under 4087
    /// bytes, over `u32::MAX`, or so large that the largest record does not fit in a
    /// `usize`.
    pub fn fit(len: u64, body_max: usize) -> Result<Self, Small> {
        let window = window(body_max).unwrap_or_else(|| {
            panic!("a body of at most {body_max} bytes makes no ring")
        });
        let min = AREA_START + area_min(window);
        if len < min {
            return Err(Small { len, min });
        }
        let area = (len - AREA_START) / BLOCK * BLOCK;
        Ok(Self {
            area,
            body_max,
            window,
        })
    }

    /// The area in bytes.
    #[must_use]
    pub fn area(self) -> u64 {
        self.area
    }

    /// The place in the area of ring offset `offset`.
    pub(crate) fn place(self, offset: u64) -> u64 {
        offset % self.area
    }

    /// The most bytes one record body holds.
    #[must_use]
    pub fn body_max(self) -> usize {
        self.body_max
    }

    /// The most bytes of parts in a batch of one entry that
    /// [`Buffer::append`](crate::Buffer::append) takes, at least
    /// [`ENTRY_MAX_MIN`](Self::ENTRY_MAX_MIN): one byte more gives
    /// [`Rejected::Large`](crate::Rejected::Large) with
    /// [`Limit::Body`](crate::Limit::Body). Each entry of a larger batch adds to the
    /// record's table, so its entries hold less in all. A shard's pool can bound an
    /// entry lower, with [`Limit::Block`](crate::Limit::Block).
    #[must_use]
    pub fn entry_max(self) -> usize {
        self.body_max - entry::table_len(1)
    }

    /// Checks a batch of `entries` entries, with `parts` parts and `bytes` bytes of
    /// parts in all, against the limits of one record of this ring, as
    /// [`Buffer::append`](crate::Buffer::append) does before it queues the batch.
    /// It does not check [`Limit::Block`](crate::Limit::Block), which depends on
    /// the pool.
    ///
    /// # Errors
    ///
    /// The first [`Limit`] the batch is over, in the order of [`Limit`]. `append`
    /// then gives [`Rejected::Large`](crate::Rejected::Large) with the same limit,
    /// unless the buffer ended with a file error, which `append` reports first.
    pub fn check(
        self,
        entries: usize,
        parts: usize,
        bytes: usize,
    ) -> Result<(), Limit> {
        if entries > ENTRIES_MAX {
            return Err(Limit::Entries { count: entries });
        }
        if parts > ENTRIES_MAX {
            return Err(Limit::Parts { count: parts });
        }
        let len = entry::table_len(entries).saturating_add(bytes);
        if len > self.body_max {
            return Err(Limit::Body {
                len,
                max: self.body_max,
            });
        }
        Ok(())
    }

    /// The length of the ring file: the two header blocks and the area.
    pub(crate) fn file_len(self) -> u64 {
        AREA_START + self.area
    }
}

/// A limit of one record, or of the pool block that holds one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// More entries than one record holds.
    Entries {
        /// The entries of the batch.
        count: usize,
    },
    /// More parts, in all the entries together, than one record holds.
    Parts {
        /// The parts of the batch.
        count: usize,
    },
    /// A record body, the entry table and the parts, over the layout's `body_max`.
    Body {
        /// Bytes of the body, or `usize::MAX` when the body is past it.
        len: usize,
        /// The layout's `body_max`.
        max: usize,
    },
    /// An entry whose parts, joined, no block of the shard's pool holds. A read
    /// gives each entry in one block.
    Block {
        /// Bytes of the entry's parts.
        len: usize,
        /// The pool's largest block payload.
        max: usize,
    },
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entries { count } => write!(
                f,
                "the batch has {count} entries, and a record holds at most \
                 {ENTRIES_MAX}"
            ),
            Self::Parts { count } => write!(
                f,
                "the batch has {count} parts, and a record holds at most {ENTRIES_MAX}"
            ),
            Self::Body { len, max } => write!(
                f,
                "the batch needs a record body of {len} bytes, and a record of this \
                 ring holds at most {max}"
            ),
            Self::Block { len, max } => write!(
                f,
                "an entry has {len} bytes of parts, and a block of the pool holds at \
                 most {max}"
            ),
        }
    }
}

/// The ring has no room for a record. Space returns with [`Writer::release`], or
/// never when the offsets left before the end are under `needed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Full {
    /// Bytes of the area that the record needs, with the rest it must skip.
    pub(crate) needed: u64,
    /// Bytes the record may take: the area not in use, or the offsets left before
    /// the end, whichever is less.
    pub(crate) free: u64,
}

/// One write to the area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    /// The place in the area.
    pub(crate) place: u64,
    /// The bytes to write there, followed by the body of the record.
    pub(crate) header: [u8; HEADER_LEN],
}

/// The boundaries after the records of one [`Plan`]: the places a trim can move the
/// tail to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ends {
    /// After the wrap record, when the record wraps.
    pub(crate) wrap: Option<Position>,
    /// After the record. The next record follows its chain value.
    pub(crate) record: Position,
}

/// The places of one record in the ring. [`seal`](Self::seal) makes its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The place of a wrap record before the record, when the record wraps.
    pub(crate) wrap: Option<u64>,
    /// The place of the record.
    pub(crate) place: u64,
    /// The offset of the record.
    pub(crate) offset: u64,
    /// The offset after the record.
    pub(crate) next: u64,
    /// The length of the body the record was placed for.
    len: usize,
}

impl Plan {
    /// Makes the headers of the data record, with `body` as its bytes, chained
    /// from `chain`, and returns them with the boundaries after its records.
    ///
    /// # Panics
    ///
    /// When `body` is not the length the record was placed for.
    pub(crate) fn seal<'a>(
        self,
        chain: u32,
        body: impl IntoIterator<Item = &'a [u8], IntoIter: Clone>,
    ) -> (Sealed, Ends) {
        self.headers(chain, Kind::Data, body)
    }

    fn headers<'a>(
        self,
        chain: u32,
        kind: Kind,
        body: impl IntoIterator<Item = &'a [u8], IntoIter: Clone>,
    ) -> (Sealed, Ends) {
        let body = body.into_iter();
        let len = body.clone().map(<[u8]>::len).sum::<usize>();
        assert!(
            len == self.len,
            "invariant: a body of {len} bytes seals a record placed for {} bytes",
            self.len
        );
        let mut chain = chain;
        let wrap = self.wrap.map(|place| {
            let (header, next) = record::header(chain, Kind::Wrap, []);
            chain = next;
            let end = Position {
                offset: self.offset,
                chain,
            };
            (Write { place, header }, end)
        });
        let (header, chain) = record::header(chain, kind, body);
        let record = Write {
            place: self.place,
            header,
        };
        let sealed = Sealed {
            wrap: wrap.map(|(write, _)| write),
            record,
        };
        let ends = Ends {
            wrap: wrap.map(|(_, end)| end),
            record: Position {
                offset: self.next,
                chain,
            },
        };
        (sealed, ends)
    }
}

/// The writes that put one record in the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sealed {
    /// A wrap record to write first. It has no body.
    pub(crate) wrap: Option<Write>,
    pub(crate) record: Write,
}

/// Plans where each record goes. It holds the live part of the ring: from the tail,
/// the oldest record still needed, to the head, where the next record starts.
/// [`Cursor::writer`] makes it.
#[derive(Debug)]
pub(crate) struct Writer {
    layout: Layout,
    tail: u64,
    head: u64,
    /// The boundary after each synced record past the tail, of any kind, oldest
    /// first: the places a trim can move the tail to. A record of one block is the
    /// smallest, so it holds at most one boundary for each block of the area.
    ends: VecDeque<Position>,
}

impl Writer {
    /// The offset where the next record starts.
    #[cfg(test)]
    pub(crate) fn head(&self) -> u64 {
        self.head
    }

    /// Plans a record with a body of `len` bytes.
    ///
    /// # Errors
    ///
    /// [`Full`] when the record does not fit before the tail. Nothing changes.
    ///
    /// # Panics
    ///
    /// When `len` is more than the layout's maximum.
    pub(crate) fn append(&mut self, len: usize) -> Result<Plan, Full> {
        let (skipped, size) = self.cost(len)?;
        let wrap = (skipped > 0).then(|| self.layout.place(self.head));
        let start = self.head + skipped;
        self.head = start + size;
        Ok(Plan {
            wrap,
            place: self.layout.place(start),
            offset: start,
            next: self.head,
            len,
        })
    }

    /// The layout of the ring.
    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    /// Checks that a record with a body of `len` bytes fits before the tail.
    /// [`append`](Self::append) with that body then succeeds.
    ///
    /// # Errors
    ///
    /// [`Full`], as `append` gives it.
    ///
    /// # Panics
    ///
    /// When `len` is more than the layout's maximum.
    pub(crate) fn fits(&self, len: usize) -> Result<(), Full> {
        self.cost(len).map(drop)
    }

    /// Frees the records before `tail`, an offset that an earlier plan gave.
    ///
    /// # Panics
    ///
    /// When `tail` is outside the live part of the ring.
    pub(crate) fn release(&mut self, tail: u64) {
        assert!(
            (self.tail..=self.head).contains(&tail),
            "invariant: release to {tail} is outside the live records from {} to {}",
            self.tail,
            self.head
        );
        self.tail = tail;
        while self.ends.front().is_some_and(|end| end.offset <= tail) {
            self.ends.pop_front();
        }
    }

    /// Records that the record with the boundaries `ends` is synced, so a trim can
    /// free it. Call it for each record, in their order.
    ///
    /// # Panics
    ///
    /// When a boundary is not after the last synced record, or is past the head.
    pub(crate) fn synced(&mut self, ends: Ends) {
        for end in ends.wrap.into_iter().chain([ends.record]) {
            let last = self.ends.back().map_or(self.tail, |end| end.offset);
            assert!(
                last < end.offset && end.offset <= self.head,
                "invariant: a record synced to {} is not after {last} and up to the \
                 head at {}",
                end.offset,
                self.head
            );
            self.ends.push_back(end);
        }
    }

    /// The tail that frees the oldest synced records, up to the first boundary
    /// that leaves the headroom free. `None` when the headroom is free, or when no
    /// record can go. The tail never passes the last synced record, or the offset
    /// `kept`: the oldest record that must stay. Once the tail is durable,
    /// [`release`](Self::release) frees the records.
    ///
    /// The headroom is three times the larger of the largest record and the records
    /// not yet synced. The space of a trim is free only at its release. From one trim
    /// to the release of the next, the ring takes the records of two commits and the
    /// blocks that one wrap skips.
    #[cfg_attr(not(test), expect(dead_code, reason = "a commit calls it"))]
    pub(crate) fn trimmed(&self, kept: Option<u64>) -> Option<Position> {
        let window = self.layout.window;
        let synced = self.ends.back().map_or(self.tail, |end| end.offset);
        let queued = self.head - synced;
        let room = queued.max(window).saturating_mul(3);
        let spare = self.layout.area.saturating_sub(room);
        let want = self.head.saturating_sub(spare);
        if want <= self.tail {
            return None;
        }
        let enough = self.ends.partition_point(|end| end.offset < want);
        let free = self
            .ends
            .partition_point(|end| kept.is_none_or(|kept| end.offset <= kept));
        let last = free.checked_sub(1)?;
        self.ends.get(enough.min(last)).copied()
    }

    /// The bytes a record with a body of `len` bytes skips at the end of the area
    /// and the bytes it takes.
    fn cost(&self, len: usize) -> Result<(u64, u64), Full> {
        assert!(
            len <= self.layout.body_max,
            "invariant: a body of {len} bytes is over the maximum of {}",
            self.layout.body_max
        );
        let size = to_u64((HEADER_LEN + len).next_multiple_of(ALIGN));
        let area = self.layout.area;
        let rest = area - self.layout.place(self.head);
        let skipped = if size > rest { rest } else { 0 };
        let live = self.head - self.tail;
        let free = (area - live).min(u64::MAX - self.head);
        let needed = skipped + size;
        if needed > free {
            return Err(Full { needed, free });
        }
        Ok((skipped, size))
    }
}

/// The bytes of the area that a [`Cursor`] reads next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) place: u64,
    pub(crate) len: usize,
}

/// One step of a [`Cursor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step<'a> {
    /// The body of the next data record.
    Data(Body<'a>),
    /// A wrap or restart record. The cursor moved; ask for the next window.
    Moved,
    /// The record goes on past the bytes given; ask for the next window.
    More,
    /// The chain ends here.
    End,
}

/// A record that follows the chain but that this version cannot read: a kind it
/// does not know, a wrap or restart record of the wrong shape, or a record that
/// ends past the end of the offsets. The ring is from another version or a defect
/// wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Invalid {
    pub(crate) offset: u64,
    pub(crate) kind: u8,
}

/// A record longer than one block, partly read.
#[derive(Clone, Copy, Debug)]
struct Partial {
    head: Head,
    /// Bytes of the record read so far, whole blocks.
    read: usize,
    /// The CRC over the bytes read.
    check: Check,
    /// `check` at [`TABLE`] bytes, or at the end of a shorter record.
    table: Check,
}

impl Partial {
    /// The record with header `head` that follows `chain`, with nothing read.
    fn new(head: Head, chain: u32) -> Self {
        let check = Check::new(head, chain);
        Self {
            head,
            read: 0,
            check,
            table: check,
        }
    }

    /// Takes the next window of the record, `bytes`, and gives its body part.
    fn feed<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let from = HEADER_LEN.saturating_sub(self.read);
        let to = (HEADER_LEN + self.head.len - self.read).min(bytes.len());
        let body = bytes
            .get(from..to)
            .expect("invariant: the body part is within the window");
        self.check.feed(body);
        self.read += bytes.len();
        if self.read <= TABLE {
            self.table = self.check;
        }
        body
    }
}

/// Where a [`Cursor`] is in the record at its offset.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// The next window is the first block of the record.
    Head,
    /// The next window continues the body of the record.
    Body(Partial),
    /// The body checked out. The next window is the start of the record again,
    /// up to [`TABLE`] bytes, which must check out as `table` once more.
    Table { head: Head, table: Check },
}

/// Walks the records of a ring from its tail at each open. Loop: read the bytes of
/// [`window`](Self::window) from the area, give them to [`next`](Self::next), and
/// stop at [`Step::End`]. Each place must read the same each time, so the caller
/// writes back each window it reads. Then [`writer`](Self::writer) continues the
/// ring. It ends within one lap of the area on any bytes.
///
/// A window is one block, or a piece of a record longer than one block, at most
/// `piece` bytes. The cursor reads such a record in three parts: its first block,
/// the rest of its body in pieces while the CRC runs, then its start again, up to
/// [`TABLE`] bytes, for the first bytes of the body. A walk reads at most twice
/// the bytes it walks, plus one block and one largest record for a torn record at
/// the end, and holds one window at a time.
#[derive(Debug)]
pub(crate) struct Cursor {
    layout: Layout,
    tail: u64,
    at: Position,
    /// The boundary after the last data record read, or the tail.
    head: Position,
    /// The boundary after each record read, of any kind, oldest first.
    ends: VecDeque<Position>,
    piece: usize,
    phase: Phase,
    ended: bool,
}

impl Cursor {
    /// Starts at `tail`, the boundary before the oldest live record. A ring that
    /// was just made has its tail at offset 0 with a random chain value. `piece`
    /// bounds a window, as the largest block the reader can hold, so
    /// [`Step::Data`] gives at least the whole entry table.
    ///
    /// # Panics
    ///
    /// When `piece` is under [`TABLE`] bytes or not a multiple of [`ALIGN`].
    pub(crate) fn new(layout: Layout, tail: Position, piece: usize) -> Self {
        assert!(
            piece >= TABLE && piece.is_multiple_of(ALIGN),
            "invariant: a piece of {piece} bytes is not whole blocks of at least {TABLE}"
        );
        Self {
            layout,
            tail: tail.offset,
            at: tail,
            head: tail,
            ends: VecDeque::new(),
            piece,
            phase: Phase::Head,
            ended: false,
        }
    }

    /// The offset of the record that [`next`](Self::next) reads.
    pub(crate) fn offset(&self) -> u64 {
        self.at.offset
    }

    pub(crate) fn window(&self) -> Window {
        let Window { place, len } = self.bound();
        match self.phase {
            Phase::Head => Window {
                place,
                len: len.min(ALIGN),
            },
            Phase::Body(Partial { head, read, .. }) => {
                let end = if read < TABLE {
                    head.size.min(TABLE)
                } else {
                    head.size.min(read + self.piece)
                };
                Window {
                    place: place + to_u64(read),
                    len: end - read,
                }
            }
            Phase::Table { head, .. } => Window {
                place,
                len: head.size.min(TABLE),
            },
        }
    }

    /// The largest window at the read position: the largest record, the rest of the
    /// area,
    /// or the rest of one lap.
    fn bound(&self) -> Window {
        let area = self.layout.area;
        let place = self.layout.place(self.at.offset);
        let len = self.layout.window.min(area - place).min(self.unread());
        let len = usize::try_from(len).expect("invariant: a window fits in memory");
        Window { place, len }
    }

    /// Bytes of the area between the read position and the tail one lap later.
    fn unread(&self) -> u64 {
        self.layout.area - (self.at.offset - self.tail)
    }

    /// Reads `bytes`, the bytes of the last [`window`](Self::window).
    ///
    /// # Errors
    ///
    /// [`Invalid`] for a record that follows the chain but cannot be read.
    ///
    /// # Panics
    ///
    /// When `bytes` is not the window, or when the start of a record reads
    /// differently the second time: the caller writes back each window it reads,
    /// so the bytes the CRC covered must come back.
    pub(crate) fn next<'a>(&mut self, bytes: &'a [u8]) -> Result<Step<'a>, Invalid> {
        let Window { place, len } = self.window();
        assert!(
            bytes.len() == len,
            "invariant: got {} bytes for a window of {len} at {place}",
            bytes.len()
        );
        match self.phase {
            Phase::Head => {
                let bound = self.bound().len;
                if let Some(head) = record::head(bytes)
                    && head.size > len
                    && head.size <= bound
                {
                    let mut partial = Partial::new(head, self.at.chain);
                    partial.feed(bytes);
                    self.phase = Phase::Body(partial);
                    return Ok(Step::More);
                }
                let Some(record) = record::read(bytes, self.at.chain) else {
                    self.ended = true;
                    return Ok(Step::End);
                };
                self.step(record)
            }
            Phase::Body(mut partial) => {
                partial.feed(bytes);
                let Partial {
                    head,
                    read,
                    check,
                    table,
                } = partial;
                if read < head.size {
                    self.phase = Phase::Body(partial);
                    return Ok(Step::More);
                }
                if !check.passes() {
                    self.phase = Phase::Head;
                    self.ended = true;
                    return Ok(Step::End);
                }
                self.phase = Phase::Table { head, table };
                Ok(Step::More)
            }
            Phase::Table { head, table } => {
                self.phase = Phase::Head;
                let mut again = Partial::new(head, self.at.chain);
                let start = again.feed(bytes);
                assert!(
                    again.check == table,
                    "invariant: the start of a checked record at {} read the same twice",
                    self.at.offset
                );
                self.step(Record {
                    kind: head.kind,
                    body: Body {
                        start,
                        len: head.len,
                    },
                    size: head.size,
                    crc: head.crc,
                })
            }
        }
    }

    /// Moves past a record whose body checked out.
    fn step<'a>(&mut self, record: Record<'a>) -> Result<Step<'a>, Invalid> {
        let Record {
            kind,
            body,
            size,
            crc,
        } = record;
        let offset = self.at.offset;
        let unread = self.unread();
        let rest = self.layout.area - self.layout.place(offset);
        let (moved, chain, step) = match (Kind::decode(kind), body.whole()) {
            (Some(Kind::Data), _) => (to_u64(size), crc, Step::Data(body)),
            (Some(Kind::Wrap), Some([])) if rest < unread => (rest, crc, Step::Moved),
            (Some(Kind::Restart), Some(&[c0, c1, c2, c3])) => {
                let chain = u32::from_le_bytes([c0, c1, c2, c3]);
                (to_u64(size), chain, Step::Moved)
            }
            _ => return Err(Invalid { offset, kind }),
        };
        let next = offset.checked_add(moved).ok_or(Invalid { offset, kind })?;
        self.at = Position {
            offset: next,
            chain,
        };
        self.ends.push_back(self.at);
        if let Step::Data(_) = step {
            self.head = self.at;
        }
        Ok(step)
    }

    /// Makes the writer that continues the ring after the last data record walked,
    /// or from the tail when the walk read none, with the records before the offset
    /// `tail` released, and seals its restart record with `chain`, a new random
    /// value, as the body. The chain continues from `chain`.
    ///
    /// # Errors
    ///
    /// [`Full`] when the restart record does not fit before `tail`. A later tail
    /// needs a new walk. No tail helps when the head is at the end of the offsets:
    /// the ring is full for good.
    ///
    /// # Panics
    ///
    /// Before [`Step::End`], or when `tail` is before the tail of the walk or past
    /// the end of its last data record.
    pub(crate) fn writer(
        self,
        tail: u64,
        chain: u32,
    ) -> Result<(Writer, Sealed), Full> {
        assert!(self.ended, "invariant: the chain ends at the last step");
        let Self { head, mut ends, .. } = self;
        // The restart record goes over the records after the last data record.
        ends.truncate(ends.partition_point(|end| end.offset <= head.offset));
        let mut writer = Writer {
            layout: self.layout,
            tail: self.tail,
            head: head.offset,
            ends,
        };
        writer.release(tail);
        let plan = writer.append(RESTART_LEN)?;
        let body = chain.to_le_bytes();
        let (sealed, ends) = plan.headers(head.chain, Kind::Restart, [&body[..]]);
        // The open syncs the restart record before the first commit, and the chain
        // continues from its body.
        let offset = plan.next;
        let record = Position { offset, chain };
        writer.synced(Ends { record, ..ends });
        Ok((writer, sealed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use env::files::SECTOR;
    use proptest::prelude::*;
    use std::{iter, mem};

    const BLOCKS: u64 = 16;
    const AREA: u64 = BLOCKS * 4096;
    const BODY_MAX: usize = 3 * ALIGN;

    /// The smallest piece the test reader holds. Every record of the fixture
    /// fits in it; the long fixture below does not.
    const PIECE: usize = TABLE;

    const START: Position = Position {
        offset: 0,
        chain: 0x5EED_0001,
    };

    fn data(body: &[u8]) -> Step<'_> {
        Step::Data(Body {
            start: body,
            len: body.len(),
        })
    }

    /// The whole body of a data record at `place` of `area`.
    fn whole(area: &[u8], place: u64, body: Body<'_>) -> Vec<u8> {
        let start = index(place) + HEADER_LEN;
        let whole = &area[start..start + body.len];
        assert!(whole.starts_with(body.start), "the start starts the body");
        whole.to_vec()
    }

    fn layout() -> Layout {
        Layout::new(AREA, BODY_MAX).expect("the test sizes make a ring")
    }

    /// The ring of 8 blocks that the recorded cases of [`ops`] ran on. They replay
    /// what they found only on it.
    fn recorded() -> Layout {
        Layout::new(8 * 4096, BODY_MAX).expect("the test sizes make a ring")
    }

    /// A ring of 32 blocks whose largest record takes 4, so the headroom of a trim
    /// is 12 blocks when every record is synced.
    fn wide() -> Layout {
        Layout::new(32 * 4096, BODY_MAX).expect("the test sizes make a ring")
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    fn at(blocks: u64, chain: u32) -> Position {
        Position::new(blocks * 4096, chain).expect("a block count is aligned")
    }

    /// Walks the area with the real cursor to the end of the chain. Checks that it
    /// asks for at most twice the bytes it walks, one block, and one largest record.
    fn walk(area: &[u8], tail: Position) -> Result<(Vec<Vec<u8>>, Cursor), Invalid> {
        walk_in(layout(), area, tail)
    }

    /// [`walk`] on a ring of `layout`.
    fn walk_in(
        layout: Layout,
        area: &[u8],
        tail: Position,
    ) -> Result<(Vec<Vec<u8>>, Cursor), Invalid> {
        let mut cursor = Cursor::new(layout, tail, PIECE);
        let mut data = Vec::new();
        let mut asked = 0;
        for _ in 0..=6 * layout.area / 4096 {
            let Window { place, len } = cursor.window();
            asked += to_u64(len);
            match cursor.next(&area[index(place)..index(place) + len])? {
                Step::Data(body) => data.push(whole(area, place, body)),
                Step::Moved | Step::More => {}
                Step::End => {
                    let walked = cursor.at.offset - tail.offset;
                    let window = (BODY_MAX + HEADER_LEN).next_multiple_of(ALIGN);
                    let most = 2 * walked + to_u64(ALIGN + window);
                    assert!(asked <= most, "asked {asked} for {walked} bytes walked");
                    return Ok((data, cursor));
                }
            }
        }
        panic!("the cursor did not end within six steps per block");
    }

    /// A live record of the model: the boundary before it and its data body.
    #[derive(Clone, Debug)]
    struct Live {
        start: Position,
        /// The boundary after the wrap record, when the record wraps.
        wrap: Option<Position>,
        /// The offset of the record, past its wrap record.
        offset: u64,
        data: Option<Vec<u8>>,
    }

    /// An area in memory, the real writer, and a model of what is live. The model
    /// keeps the chain at the head: the writer has none.
    #[derive(Debug)]
    struct Ring {
        layout: Layout,
        area: Vec<u8>,
        writer: Writer,
        tail: Position,
        head: Position,
        live: VecDeque<Live>,
    }

    impl Ring {
        /// A ring that was just made and opened: it holds one restart record.
        fn new() -> Self {
            Self::with(layout())
        }

        /// [`new`](Self::new) for a ring of `layout`.
        fn with(layout: Layout) -> Self {
            let area = vec![0; index(layout.area)];
            let (_, cursor) =
                walk_in(layout, &area, START).expect("a zeroed area is valid");
            let (writer, sealed) = cursor.writer(0, 1).expect("the ring is empty");
            let mut ring = Self {
                layout,
                area,
                writer,
                tail: START,
                head: START,
                live: VecDeque::new(),
            };
            ring.restart(&sealed, 1);
            ring
        }

        /// Writes the restart record `sealed`, whose body is `chain`.
        fn restart(&mut self, sealed: &Sealed, chain: u32) {
            self.apply(sealed, &chain.to_le_bytes(), false);
            self.head = Position {
                offset: self.writer.head(),
                chain,
            };
        }

        /// Writes `bytes` at `place`. `used` bytes from the tail are live.
        fn write(&mut self, place: u64, bytes: &[u8], used: u64) {
            let len = to_u64(bytes.len());
            let area = self.layout.area;
            assert_eq!(place % 4096, 0, "a write starts off a block boundary");
            assert!(place + len <= area, "a write runs past the area");
            let first = self.tail.offset % area;
            let to_end = used.min(area - first);
            for (start, live) in [(first, to_end), (0, used - to_end)] {
                assert!(
                    place + len <= start || start + live <= place,
                    "a write of {len} bytes at {place} covers live bytes at {start}"
                );
            }
            self.area[index(place)..index(place + len)].copy_from_slice(bytes);
        }

        /// Writes the sealed record from the head, with `body` after its header.
        fn apply(&mut self, sealed: &Sealed, body: &[u8], data: bool) {
            let start = self.head;
            let used = start.offset - self.tail.offset;
            if let Some(wrap) = sealed.wrap {
                self.write(wrap.place, &wrap.header, used);
            }
            let mut bytes = sealed.record.header.to_vec();
            bytes.extend_from_slice(body);
            self.write(sealed.record.place, &bytes, used);
            let skipped = sealed.wrap.map_or(0, |wrap| self.layout.area - wrap.place);
            let offset = start.offset + skipped;
            let (_, chain) = record::header(start.chain, Kind::Wrap, []);
            let wrap = sealed.wrap.map(|_| Position { offset, chain });
            let data = data.then(|| body.to_vec());
            self.live.push_back(Live {
                start,
                wrap,
                offset,
                data,
            });
        }

        fn append(&mut self, body: &[u8]) -> Result<Plan, Full> {
            let plan = self.writer.append(body.len())?;
            let area = self.layout.area;
            assert_eq!(plan.offset % area, plan.place, "the offset is at the place");
            assert!(
                plan.offset >= self.head.offset,
                "the offset is at or past the head"
            );
            let (sealed, ends) = plan.seal(self.head.chain, [body]);
            self.apply(&sealed, body, true);
            self.head = ends.record;
            self.writer.synced(ends);
            Ok(plan)
        }

        fn release(&mut self, count: usize) {
            self.live.drain(..count.min(self.live.len()));
            self.tail = self.live.front().map_or(self.head, |live| live.start);
            self.writer.release(self.tail.offset);
        }

        /// Each record boundary after the tail, oldest first: after each wrap record
        /// and after each record, of any kind.
        fn bounds(&self) -> Vec<Position> {
            let starts = self.live.iter().skip(1).map(|live| live.start);
            let ends = starts.chain([self.head]);
            let records = self.live.iter().zip(ends);
            let bounds =
                records.flat_map(|(live, end)| live.wrap.into_iter().chain([end]));
            bounds
                .filter(|bound| bound.offset > self.tail.offset)
                .collect()
        }

        /// Trims as a commit does: moves the tail where the writer says and frees
        /// the records before it. `kept` is the offset of a record that must stay.
        fn trim(&mut self, kept: Option<u64>) -> Option<Position> {
            let tail = self.writer.trimmed(kept)?;
            assert!(
                self.bounds().contains(&tail),
                "a trim to {tail:?}, which is not after a record"
            );
            self.live.retain(|live| live.offset >= tail.offset);
            self.tail = tail;
            self.writer.release(tail.offset);
            Some(tail)
        }

        /// Trims and checks the tail against the records of the model, which are
        /// all synced: the headroom is three of the largest record. The tail is the
        /// first boundary that leaves it free, or the last one that `kept` permits.
        fn check_trim(&mut self, kept: Option<u64>) {
            let (area, head, old) = (self.layout.area, self.head.offset, self.tail);
            let room = 3 * self.layout.window;
            let short = |tail: u64| area - (head - tail) < room;
            let bounds = self.bounds();
            let free = bounds.iter();
            let free: Vec<&Position> = free
                .filter(|bound| kept.is_none_or(|kept| bound.offset <= kept))
                .collect();
            match self.trim(kept) {
                None => assert!(
                    !short(old.offset) || free.is_empty(),
                    "no trim from {old:?} that keeps {kept:?}"
                ),
                Some(tail) => {
                    assert!(short(old.offset), "a trim with the headroom free");
                    let found = free.iter().position(|bound| **bound == tail);
                    let found = found.expect("a trim past the record that must stay");
                    let before =
                        found.checked_sub(1).map_or(old, |before| *free[before]);
                    assert!(short(before.offset), "a trim past {before:?} to {tail:?}");
                    assert!(
                        !short(tail.offset) || found + 1 == free.len(),
                        "a trim to {tail:?} that stops short"
                    );
                }
            }
            let live = self.live.iter().map(|live| live.offset);
            assert!(
                kept.is_none_or(|kept| live.clone().any(|offset| offset == kept)),
                "a trim freed the record at {kept:?}"
            );
            assert_eq!(self.walk().0, self.data(), "data after a trim");
        }

        fn data(&self) -> Vec<Vec<u8>> {
            let data = self.live.iter();
            data.filter_map(|live| live.data.clone()).collect()
        }

        /// The boundary after the last live data record: the start of the restart
        /// records after it, or the head.
        fn after_data(&self) -> Position {
            let restarts = self.live.iter().rev().take_while(|l| l.data.is_none());
            restarts.last().map_or(self.head, |live| live.start)
        }

        fn walk(&self) -> (Vec<Vec<u8>>, Cursor) {
            walk_in(self.layout, &self.area, self.tail).expect("a valid ring")
        }

        /// Drops the writer, as a crash does, walks the area, and continues with a
        /// new writer. Its restart record goes over the restart records after the
        /// last data record. Gives the data that the walk found.
        fn reopen(&mut self, chain: u32) -> Result<Vec<Vec<u8>>, Full> {
            let (data, cursor) = self.walk();
            let head = cursor.head;
            let (writer, sealed) = cursor.writer(self.tail.offset, chain)?;
            while self.live.back().is_some_and(|live| live.data.is_none()) {
                self.live.pop_back();
            }
            self.writer = writer;
            self.head = head;
            self.restart(&sealed, chain);
            Ok(data)
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Append(Vec<u8>),
        Reopen(u32),
        Release(usize),
        /// A trim that keeps the live data record at this index.
        Trim(Option<usize>),
    }

    fn body() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            prop::collection::vec(any::<u8>(), 0..=BODY_MAX),
        ]
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            4 => body().prop_map(Op::Append),
            1 => any::<u32>().prop_map(Op::Reopen),
            2 => (0..4usize).prop_map(Op::Release),
        ];
        prop::collection::vec(op, 0..40)
    }

    /// Appends, reopens, and trims. The recorded cases of [`ops`] keep their
    /// meaning because it has no trim.
    fn trims() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            4 => body().prop_map(Op::Append),
            1 => any::<u32>().prop_map(Op::Reopen),
            2 => prop::option::of(0..4usize).prop_map(Op::Trim),
        ];
        prop::collection::vec(op, 0..40)
    }

    /// Runs `ops` and checks each refusal and each reopen against the model. A
    /// ring with no live data takes any record.
    fn run(ring: &mut Ring, ops: &[Op]) {
        for op in ops {
            let head = ring.writer.head();
            let after = ring.after_data();
            let area = ring.layout.area;
            let free = match op {
                Op::Reopen(_) => area - (after.offset - ring.tail.offset),
                _ => area - (head - ring.tail.offset),
            };
            let live = ring.data();
            let result = match op {
                Op::Append(body) => ring.append(body).map(drop),
                Op::Reopen(chain) => ring.reopen(*chain).map(|data| {
                    assert_eq!(data, live, "data found at a reopen");
                    let restart = ring.live.back().map(|live| live.start);
                    assert_eq!(restart, Some(after), "the restart record's place");
                }),
                Op::Release(count) => {
                    ring.release(*count);
                    Ok(())
                }
                Op::Trim(pick) => {
                    let mut data = ring.live.iter().filter(|live| live.data.is_some());
                    let kept = pick.and_then(|pick| data.nth(pick));
                    ring.check_trim(kept.map(|live| live.offset));
                    Ok(())
                }
            };
            if let Err(full) = result {
                assert_eq!(full.free, free, "free bytes at {op:?}");
                assert!(full.needed > free, "a record that fits was refused");
                assert_eq!(ring.writer.head(), head, "a refusal moved the head");
                assert!(!live.is_empty(), "a ring with no data refused {op:?}");
            }
        }
    }

    mod position {
        use super::*;

        #[test]
        fn takes_only_an_offset_on_a_block_boundary() {
            let position = Position::new(8192, 7).expect("8192 is aligned");
            assert_eq!((position.offset(), position.chain()), (8192, 7));
            assert_eq!(Position::new(8193, 7), Err(Unaligned { offset: 8193 }));
        }
    }

    mod layout {
        use super::*;

        #[test]
        fn refuses_sizes_that_do_not_make_a_ring() {
            let block = 4096;
            let cases = [
                ("an area of part blocks", 7 * block + 1, 4087),
                ("a body under one block less the header", 8 * block, 4086),
                ("an area of one record of two blocks", 2 * block, 4088),
                ("an area of one record of one block", block, 4087),
                ("an area of two records less a block", 3 * block, 4088),
                ("a body over u32::MAX", u64::MAX - 4095, usize::MAX),
                ("a record size over u64", u64::MAX - 4095, usize::MAX - 8),
                ("a file over u64", u64::MAX - 4095, 4087),
                ("a file one byte over u64", u64::MAX - 8191, 4087),
            ];
            for (case, area, body_max) in cases {
                let unfit = Unfit { area, body_max };
                assert_eq!(Layout::new(area, body_max), Err(unfit), "{case}");
            }
        }

        #[test]
        fn takes_the_largest_aligned_area() {
            let area = u64::MAX - 12287;
            assert_eq!(
                Layout::new(area, 4087).map(Layout::file_len),
                Ok(u64::MAX - 4095)
            );
        }

        #[test]
        fn checks_a_batch_against_each_limit_at_its_boundary() {
            let layout = Layout::new(64 * 4096, 60_000).expect("a ring of 64 blocks");
            let body = |len| Limit::Body { len, max: 60_000 };
            let cases = [
                ("no entry", (0, 0, 0), Ok(())),
                ("the most entries", (1023, 0, 0), Ok(())),
                (
                    "one entry too many",
                    (1024, 0, 0),
                    Err(Limit::Entries { count: 1024 }),
                ),
                ("the most parts", (1023, 1023, 0), Ok(())),
                (
                    "one part too many",
                    (1, 1024, 0),
                    Err(Limit::Parts { count: 1024 }),
                ),
                ("a body of body_max", (1, 1, 59_945), Ok(())),
                ("a body one byte over", (1, 1, 59_946), Err(body(60_001))),
                (
                    "a table alone over the body",
                    (1023, 0, 10_000),
                    Err(body(62_177)),
                ),
                (
                    "over the entries and the parts",
                    (1024, 2048, 0),
                    Err(Limit::Entries { count: 1024 }),
                ),
                (
                    "over the parts and the body",
                    (1023, 1024, 100_000),
                    Err(Limit::Parts { count: 1024 }),
                ),
                (
                    "bytes past usize",
                    (1, 1, usize::MAX),
                    Err(body(usize::MAX)),
                ),
            ];
            for (case, (entries, parts, bytes), expected) in cases {
                assert_eq!(layout.check(entries, parts, bytes), expected, "{case}");
            }
        }

        #[test]
        fn holds_an_entry_of_4032_bytes_at_the_smallest_body() {
            assert_eq!(Layout::ENTRY_MAX_MIN, 4032);
            assert_eq!(
                Layout::new(4 * 4096, 4087).map(Layout::entry_max),
                Ok(Layout::ENTRY_MAX_MIN)
            );
        }

        #[test]
        fn takes_an_area_of_two_records() {
            assert_eq!(
                Layout::new(2 * 4096, 4087).map(|layout| layout.window),
                Ok(4096)
            );
            assert_eq!(Layout::new(4 * 4096, 4088).map(|l| l.window), Ok(8192));
        }

        #[test]
        fn fits_the_smallest_ring_in_its_file_and_no_ring_in_a_byte_less() {
            let min = 4 * 4096;
            assert_eq!(Layout::fit(min, 4087).map(Layout::file_len), Ok(min));
            for len in [min - 1, 8191, 0] {
                assert_eq!(Layout::fit(len, 4087), Err(Small { len, min }));
            }
            assert_eq!(
                Small { len: 8191, min }.to_string(),
                "a ring file of 8191 bytes holds no ring; it needs at least 16384 bytes"
            );
            let _: &dyn std::error::Error = &Small { len: 8191, min };
        }

        #[test]
        fn fits_the_largest_file() {
            assert_eq!(
                Layout::fit(u64::MAX, 4087).map(Layout::file_len),
                Ok(u64::MAX - 4095)
            );
        }

        #[test]
        #[should_panic(expected = "a body of at most 4086 bytes makes no ring")]
        fn panics_on_a_body_under_a_block() {
            let _layout = Layout::fit(u64::MAX, 4086);
        }

        #[test]
        #[should_panic(expected = "a body of at most 4294967296 bytes makes no ring")]
        fn panics_on_a_body_over_u32() {
            let _layout = Layout::fit(u64::MAX, 1 << 32);
        }

        proptest! {
            /// A fit takes the most whole blocks of `len`, and refuses each `len`
            /// under one least length, which is the file of a ring.
            #[test]
            fn fits_the_largest_ring_in_len(
                len in prop_oneof![0..32 * 4096u64, 0..u64::MAX - 4096],
                body_max in BODY_MIN..=3 * ALIGN - HEADER_LEN,
            ) {
                let min = Layout::fit(0, body_max).expect_err("no ring in 0 bytes").min;
                let under = min - 8192 - 4096;
                prop_assert_eq!(
                    Layout::new(under, body_max),
                    Err(Unfit { area: under, body_max })
                );
                let fit = Layout::fit(min, body_max);
                prop_assert_eq!(fit.map(Layout::file_len), Ok(min));
                match Layout::fit(len, body_max) {
                    Ok(layout) => {
                        prop_assert!(min <= len);
                        let new = Layout::new(layout.area(), body_max);
                        prop_assert_eq!(new, Ok(layout));
                        prop_assert!(layout.file_len() <= len);
                        prop_assert!(layout.file_len() + 4096 > len);
                    }
                    Err(small) => {
                        prop_assert!(len < min);
                        prop_assert_eq!(small, Small { len, min });
                    }
                }
            }
        }

        proptest! {
            /// The smallest area is twice the largest record. A ring of that area
            /// that holds only its restart record takes its largest record,
            /// wherever the restart record is.
            #[test]
            fn takes_the_largest_record_after_the_restart_record_at_any_tail(
                body_max in BODY_MIN..=3 * ALIGN - HEADER_LEN,
                tail in 0..64u64,
            ) {
                let record = to_u64((HEADER_LEN + body_max).next_multiple_of(ALIGN));
                let under = 2 * record - 4096;
                let unfit = Unfit { area: under, body_max };
                prop_assert_eq!(Layout::new(under, body_max), Err(unfit));
                let layout =
                    Layout::new(2 * record, body_max).expect("the smallest area");
                let mut cursor = Cursor::new(layout, at(tail, 0), PIECE);
                let zeros = vec![0; cursor.window().len];
                prop_assert_eq!(cursor.next(&zeros), Ok(Step::End));
                let (mut writer, _) =
                    cursor.writer(tail * 4096, 1).expect("the ring is empty");
                prop_assert_eq!(writer.append(body_max).map(drop), Ok(()));
            }
        }
    }

    mod writer {
        use super::*;

        /// A writer with its tail and head at block counts, after its restart
        /// record of one block.
        fn writer(tail: u64, head: u64) -> Writer {
            let mut cursor = Cursor::new(layout(), at(head - 1, 9), PIECE);
            let zeros = vec![0; cursor.window().len];
            assert_eq!(cursor.next(&zeros), Ok(Step::End));
            let mut cursor = Cursor {
                tail: tail * 4096,
                ..cursor
            };
            cursor.ended = true;
            cursor.writer(tail * 4096, 9).expect("the ring has room").0
        }

        proptest! {
            #[test]
            fn fits_agrees_with_append(
                tail in 0..BLOCKS,
                used in 1..=BLOCKS,
                len in 0..=BODY_MAX,
            ) {
                let head = tail + used;
                let mut writer = writer(tail, head);
                let fits = writer.fits(len);
                let appended = writer.append(len).map(drop);
                prop_assert_eq!(fits, appended);
            }
        }

        #[test]
        fn places_records_back_to_back_from_the_head() {
            let mut writer = writer(0, 1);
            let first = writer.append(1).expect("the ring has room");
            let second = writer.append(ALIGN).expect("the ring has room");
            let plan = |wrap, place, offset, next, len| Plan {
                wrap,
                place,
                offset,
                next,
                len,
            };
            assert_eq!(first, plan(None, 4096, 4096, 2 * 4096, 1));
            assert_eq!(second, plan(None, 2 * 4096, 2 * 4096, 4 * 4096, ALIGN));
            assert_eq!(writer.head(), second.next);
        }

        #[test]
        fn fills_a_block_with_a_body_of_4087_bytes() {
            let mut writer = writer(0, 1);
            let fits = writer.append(4087).expect("the ring has room");
            let spills = writer.append(4088).expect("the ring has room");
            assert_eq!(fits.next, 2 * 4096);
            assert_eq!(spills.next, 4 * 4096);
        }

        #[test]
        fn puts_a_wrap_record_when_a_record_does_not_fit_in_the_rest() {
            let mut writer = writer(14, 15);
            let plan = writer.append(ALIGN).expect("the ring has room");
            assert_eq!(
                (plan.wrap, plan.place, plan.next),
                (Some(15 * 4096), 0, 18 * 4096)
            );
            let (sealed, ends) = plan.seal(9, [[7; ALIGN].as_slice()]);
            let (wrap, after_wrap) = record::header(9, Kind::Wrap, []);
            let (header, after) =
                record::header(after_wrap, Kind::Data, [[7; ALIGN].as_slice()]);
            let expected = Sealed {
                wrap: Some(Write {
                    place: 15 * 4096,
                    header: wrap,
                }),
                record: Write { place: 0, header },
            };
            let boundaries = Ends {
                wrap: Some(at(16, after_wrap)),
                record: at(18, after),
            };
            assert_eq!((sealed, ends), (expected, boundaries));
        }

        /// Two records placed before either is sealed read back when each is
        /// sealed from the chain value of the one before, and the second does
        /// not when it is sealed from the first's chain.
        #[test]
        fn seal_chains_each_record_from_the_one_before() {
            let mut ring = Ring::new();
            let first = ring.writer.append(1).expect("the ring has room");
            let second = ring.writer.append(1).expect("the ring has room");
            let (a, ends) = first.seal(ring.head.chain, [b"a".as_slice()]);
            let (b, _) = second.seal(ends.record.chain, [b"b".as_slice()]);
            for (sealed, body) in [(a, b"a"), (b, b"b")] {
                let mut bytes = sealed.record.header.to_vec();
                bytes.extend_from_slice(body);
                ring.write(sealed.record.place, &bytes, 0);
            }
            let (data, _) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!(data, [b"a", b"b"]);
            let (wrong, _) = second.seal(ring.head.chain, [b"b".as_slice()]);
            let mut bytes = wrong.record.header.to_vec();
            bytes.extend_from_slice(b"b");
            ring.write(wrong.record.place, &bytes, 0);
            let (data, _) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!(data, [b"a"]);
        }

        #[test]
        #[should_panic(
            expected = "a body of 2 bytes seals a record placed for 1 bytes"
        )]
        fn seal_panics_on_a_body_of_another_length() {
            let plan = writer(0, 1).append(1).expect("the ring has room");
            let _sealed = plan.seal(0, [b"ab".as_slice()]);
        }

        #[test]
        #[should_panic(
            expected = "a body of 0 bytes seals a record placed for 1 bytes"
        )]
        fn seal_panics_on_a_body_shorter_than_the_record() {
            let plan = writer(0, 1).append(1).expect("the ring has room");
            let _sealed = plan.seal(0, []);
        }

        /// The writer of an empty ring with its tail at `offset`, after its restart
        /// record of one block.
        fn opened(layout: Layout, offset: u64) -> Writer {
            let tail = Position::new(offset, 9).expect("aligned");
            let mut cursor = Cursor::new(layout, tail, PIECE);
            let zeros = vec![0; cursor.window().len];
            assert_eq!(cursor.next(&zeros), Ok(Step::End));
            cursor.writer(offset, 9).expect("the restart record fits").0
        }

        #[test]
        fn is_full_at_the_end_of_the_offsets() {
            let mut writer = opened(layout(), u64::MAX - 12287);
            let plan = writer.append(8);
            assert_eq!(plan.map(|plan| plan.next), Ok(u64::MAX - 4095));
            let full = Full {
                needed: 4096,
                free: 4095,
            };
            assert_eq!(writer.append(8), Err(full));
        }

        #[test]
        fn is_full_when_the_wrap_passes_the_end_of_the_offsets() {
            let small = Layout::new(15 * 4096, 4088).expect("the sizes make a ring");
            let mut writer = opened(small, u64::MAX - 12287);
            let full = Full {
                needed: 3 * 4096,
                free: 8191,
            };
            assert_eq!(writer.append(4088), Err(full));
            let plan = writer.append(8);
            assert_eq!(plan.map(|plan| plan.next), Ok(u64::MAX - 4095));
        }

        #[test]
        fn wraps_before_the_end_of_the_offsets() {
            let mut writer = opened(layout(), u64::MAX - 73727);
            let plan = writer.append(ALIGN).expect("fits");
            assert_eq!(plan.wrap, Some(61440));
            assert_eq!(plan.place, 0);
            assert_eq!(plan.next, u64::MAX - 57343);
        }

        #[test]
        fn refuses_a_record_that_does_not_fit_before_the_tail() {
            let mut writer = writer(1, 15);
            let head = writer.head();
            let full = Full {
                needed: 3 * 4096,
                free: 2 * 4096,
            };
            assert_eq!(writer.append(ALIGN), Err(full));
            assert_eq!(writer.head(), head);
            writer.release(2 * 4096);
            let plan = writer.append(ALIGN);
            assert_eq!(plan.map(|plan| plan.place), Ok(0));
        }

        /// The writer of a ring of 16 blocks whose records each take one block.
        /// After its restart record, record `n` of `records` ends at block `n + 1`
        /// with the chain value `n`, and is synced.
        fn filled(records: u32) -> Writer {
            let layout = Layout::new(16 * 4096, 4087).expect("the sizes make a ring");
            let mut writer = opened(layout, 0);
            for chain in 1..=records {
                let ends = queue(&mut writer, 8, chain).expect("the ring has room");
                writer.synced(ends);
            }
            writer
        }

        /// Places a record of `len` bytes and gives its boundaries, with `chain`
        /// after the record.
        fn queue(writer: &mut Writer, len: usize, chain: u32) -> Result<Ends, Full> {
            let plan = writer.append(len)?;
            let wrap = plan.wrap.map(|_| Position {
                offset: plan.offset,
                chain: !chain,
            });
            let offset = plan.next;
            let record = Position { offset, chain };
            Ok(Ends { wrap, record })
        }

        #[test]
        fn trimmed_gives_no_tail_while_three_of_the_largest_record_fit() {
            assert_eq!(filled(12).trimmed(None), None);
        }

        #[test]
        fn trimmed_frees_records_until_three_of_the_largest_record_fit() {
            assert_eq!(filled(13).trimmed(None), Some(at(1, 9)));
            assert_eq!(filled(14).trimmed(None), Some(at(2, 1)));
        }

        #[test]
        fn trimmed_leaves_three_times_the_records_not_yet_synced_free() {
            let mut writer = filled(11);
            let queued: Vec<Ends> = (12..=14)
                .map(|chain| queue(&mut writer, 8, chain).expect("the ring has room"))
                .collect();
            assert_eq!(writer.trimmed(None), Some(at(8, 7)));
            for ends in queued {
                writer.synced(ends);
            }
            assert_eq!(writer.trimmed(None), Some(at(2, 1)));
        }

        #[test]
        fn trimmed_stops_at_the_last_synced_record() {
            let mut writer = filled(2);
            for chain in 3..=15 {
                queue(&mut writer, 8, chain).expect("the ring has room");
            }
            assert_eq!(writer.trimmed(None), Some(at(3, 2)));
            writer.release(3 * 4096);
            assert_eq!(writer.trimmed(None), None);
        }

        #[test]
        fn trimmed_stops_at_the_record_that_must_stay() {
            let mut writer = filled(15);
            assert_eq!(writer.trimmed(None), Some(at(3, 2)));
            assert_eq!(writer.trimmed(Some(2 * 4096)), Some(at(2, 1)));
            assert_eq!(writer.trimmed(Some(4096)), Some(at(1, 9)));
            writer.release(4096);
            assert_eq!(writer.trimmed(Some(4096)), None);
        }

        /// The skipped blocks of a wrap are enough for the headroom, so the trim
        /// frees the wrap record and keeps the record after it.
        #[test]
        fn trimmed_frees_a_wrap_record_and_keeps_its_record() {
            let mut ring = Ring::with(wide());
            let long = [7; BODY_MAX];
            let fill = |ring: &mut Ring| {
                ring.append(&long).expect("the ring has room");
                ring.trim(None).map(|tail| tail.offset / 4096)
            };
            let tails: Vec<Option<u64>> = (0..8).map(|_| fill(&mut ring)).collect();
            let (first, last) = tails.split_at(4);
            assert_eq!(first, [None; 4]);
            assert_eq!(last, [Some(1), Some(5), Some(9), Some(17)]);
            let wrapped = ring.live.back().expect("a record").clone();
            assert_eq!(
                (wrapped.start.offset, wrapped.offset),
                (29 * 4096, 32 * 4096)
            );
            let tails: Vec<Option<u64>> = (0..3).map(|_| fill(&mut ring)).collect();
            assert_eq!(tails, [Some(21), Some(25), Some(29)]);
            ring.append(&[7; ALIGN]).expect("the ring has room");
            assert_eq!(ring.trim(None), wrapped.wrap);
            assert_eq!(ring.walk().0.len(), 5);
        }

        /// Runs commits as the commit task does, and gives the first refusal with
        /// the count of commits before it. A commit syncs the records placed since
        /// the commit before it. Its trim is free only when its sync ends, so the
        /// records of `during` come before the release and those of `after` come
        /// after it.
        fn steady(
            layout: Layout,
            commits: impl IntoIterator<Item = (Vec<usize>, Vec<usize>)>,
        ) -> Option<(usize, Full)> {
            let mut writer = opened(layout, 0);
            let mut queued = Vec::new();
            for (commit, (during, after)) in commits.into_iter().enumerate() {
                let tail = writer.trimmed(None);
                let synced = mem::take(&mut queued);
                for len in during {
                    match queue(&mut writer, len, 1) {
                        Ok(ends) => queued.push(ends),
                        Err(full) => return Some((commit, full)),
                    }
                }
                for ends in synced {
                    writer.synced(ends);
                }
                if let Some(tail) = tail {
                    writer.release(tail.offset);
                }
                for len in after {
                    match queue(&mut writer, len, 1) {
                        Ok(ends) => queued.push(ends),
                        Err(full) => return Some((commit, full)),
                    }
                }
            }
            None
        }

        #[test]
        fn a_record_placed_during_the_sync_of_a_trim_is_not_refused() {
            let layout = Layout::new(64 * 4096, BODY_MAX).expect("a ring");
            let commits = (0..1000).map(|_| (vec![BODY_MAX], vec![]));
            assert_eq!(steady(layout, commits), None);
        }

        #[test]
        fn a_steady_load_loses_no_record_to_a_full_ring() {
            let layout = Layout::new(16 * 4096, BODY_MAX).expect("a ring");
            let lens = [BODY_MAX, BODY_MAX, 2 * ALIGN];
            let commits = (0..100_000).map(|commit| (vec![lens[commit % 3]], vec![]));
            assert_eq!(steady(layout, commits), None);
        }

        /// Each commit holds `counts` records of `len` bytes, in turn, and each
        /// record comes while the commit before it syncs.
        fn turns(blocks: u64, counts: [usize; 2], len: usize) -> Option<(usize, Full)> {
            let layout = Layout::new(blocks * 4096, BODY_MAX).expect("a ring");
            let commits =
                (0..1000).map(|commit| (vec![len; counts[commit % 2]], vec![]));
            steady(layout, commits)
        }

        #[test]
        fn a_commit_larger_than_the_last_is_not_refused() {
            assert_eq!(turns(256, [5, 7], BODY_MAX), None);
            assert_eq!(turns(256, [5, 6], BODY_MAX), None);
            assert_eq!(turns(1024, [50, 52], BODY_MAX), None);
            assert_eq!(turns(256, [20, 28], 8), None);
        }

        /// Fills a ring of 1024 blocks with commits of 40 records of one block, then
        /// runs the two commits of `then`. Each record comes while the commit
        /// before it syncs.
        fn stepped(then: [usize; 2]) -> Option<(usize, Full)> {
            let layout = Layout::new(1024 * 4096, BODY_MAX).expect("a ring");
            let counts = iter::repeat_n(40, 400).chain(then);
            steady(layout, counts.map(|count| (vec![8; count], vec![])))
        }

        /// The headroom of a trim is free at its release, after the next commit is
        /// in the ring and before the one after it.
        #[test]
        fn the_two_commits_after_a_trim_fit_in_three_times_its_commit() {
            assert_eq!(stepped([80, 40]), None);
            assert_eq!(stepped([60, 60]), None);
            let full = Full {
                needed: 4096,
                free: 0,
            };
            assert_eq!(stepped([81, 40]), Some((400, full)));
            assert_eq!(stepped([60, 61]), Some((401, full)));
        }

        /// Fills a ring of 1024 blocks with `fill` commits of 40 records of one
        /// block, then runs a commit of `count` largest records and a commit of 40
        /// records of one block. Each record comes while the commit before it syncs.
        fn jumped(fill: usize, count: usize) -> Option<(usize, Full)> {
            let layout = Layout::new(1024 * 4096, BODY_MAX).expect("a ring");
            let then = [vec![BODY_MAX; count], vec![8; 40]];
            let commits = iter::repeat_n(vec![8; 40], fill).chain(then);
            steady(layout, commits.map(|lens| (lens, vec![])))
        }

        #[test]
        fn a_wrap_takes_the_blocks_that_it_skips_from_the_headroom() {
            assert_eq!(jumped(40, 20), None);
            assert_eq!(jumped(51, 19), None);
            let full = Full {
                needed: 16384,
                free: 4096,
            };
            assert_eq!(jumped(51, 20), Some((51, full)));
        }

        /// A trim cannot free the commit in its sync, so the ring must hold three
        /// commits in a row: three times a steady commit, and four times it when one
        /// commit is twice the commit before it.
        #[test]
        fn a_ring_that_does_not_hold_the_commits_of_the_headroom_refuses_a_record() {
            let layout = Layout::new(1024 * 4096, BODY_MAX).expect("a ring");
            let full = Full {
                needed: 4096,
                free: 0,
            };
            let even = |count: usize| {
                steady(layout, (0..1000).map(|_| (vec![8; count], vec![])))
            };
            assert_eq!(even(341), None);
            assert_eq!(even(342), Some((2, full)));
            let doubled = |count: usize| {
                let counts = iter::repeat_n(count, 400).chain([2 * count, count]);
                steady(layout, counts.map(|count| (vec![8; count], vec![])))
            };
            assert_eq!(doubled(256), None);
            assert_eq!(doubled(257), Some((400, full)));
        }

        /// The ring must also hold the blocks that a wrap skips in those commits. The
        /// first record takes one block, so only the first wrap must skip: 3 blocks.
        /// With 1 to 5 commits before the doubled one, that wrap is in the commit after
        /// it, in it (2 and 3), in the commit before it, or in the one two before it.
        #[test]
        fn a_ring_that_holds_three_commits_and_no_wrap_skip_refuses_a_record() {
            let layout = Layout::new(1024 * 4096, BODY_MAX).expect("a ring");
            let doubled = |before: usize| {
                let ends = [vec![BODY_MAX; 128], vec![BODY_MAX; 64]];
                let commits = iter::repeat_n(vec![BODY_MAX; 64], before).chain(ends);
                steady(layout, commits.map(|lens| (lens, vec![])))
            };
            let got: Vec<_> = (0..=7).map(doubled).collect();
            let full = |commit| {
                let (needed, free) = (16384, 4096);
                Some((commit, Full { needed, free }))
            };
            let (needed, free) = (28672, 16384);
            let first = Some((2, Full { needed, free }));
            let skipped = [first, first, full(3), full(4), full(5)];
            assert_eq!(got, [vec![None], skipped.to_vec(), vec![None; 2]].concat());
        }

        /// Four of the largest record hold three commits of one record, not of two.
        #[test]
        fn a_ring_of_four_records_refuses_a_steady_load_of_two_records_a_commit() {
            let layout = Layout::new(16 * 4096, BODY_MAX).expect("a ring");
            let commits = (0..1000).map(|_| (vec![BODY_MAX, ALIGN], vec![]));
            let full = Full {
                needed: 28672,
                free: 16384,
            };
            assert_eq!(steady(layout, commits), Some((2, full)));
        }

        /// A body length whose record takes 1 to 4 blocks, each count as likely.
        fn bodies() -> impl Strategy<Value = usize> {
            (1..=4usize, 0..ALIGN).prop_map(|(blocks, less)| {
                let most = blocks * ALIGN - HEADER_LEN;
                most.saturating_sub(less).min(BODY_MAX)
            })
        }

        proptest! {
            /// A ring of four of its largest record refuses no record when each
            /// commit holds one, of any length, placed before or after the release
            /// of the trim before it.
            #[test]
            fn a_ring_of_four_records_refuses_no_record_of_a_steady_load(
                extra in 0..8u64,
                lens in prop::collection::vec((bodies(), any::<bool>()), 0..400),
            ) {
                let area = (16 + extra) * 4096;
                let layout = Layout::new(area, BODY_MAX).expect("a ring");
                let commits = lens.into_iter().map(|(len, late)| {
                    if late { (vec![], vec![len]) } else { (vec![len], vec![]) }
                });
                prop_assert_eq!(steady(layout, commits), None);
            }
        }

        #[test]
        #[should_panic(
            expected = "a record synced to 4096 is not after 4096 and up to the head \
                        at 8192"
        )]
        fn panics_on_a_synced_record_that_is_not_after_the_last() {
            let mut writer = opened(layout(), 0);
            queue(&mut writer, 8, 1).expect("the ring has room");
            let (wrap, record) = (None, at(1, 1));
            writer.synced(Ends { wrap, record });
        }

        #[test]
        #[should_panic(
            expected = "a record synced to 8192 is not after 4096 and up to the head \
                        at 4096"
        )]
        fn panics_on_a_synced_record_past_the_head() {
            let (wrap, record) = (None, at(2, 1));
            opened(layout(), 0).synced(Ends { wrap, record });
        }

        #[test]
        #[should_panic(expected = "a body of 12289 bytes is over the maximum of 12288")]
        fn panics_on_a_body_over_the_maximum() {
            let _plan = writer(0, 1).append(BODY_MAX + 1);
        }

        #[test]
        #[should_panic(expected = "release to 8192 is outside the live records from 0")]
        fn panics_on_a_release_past_the_head() {
            writer(0, 1).release(2 * 4096);
        }

        #[test]
        #[should_panic(
            expected = "release to 4096 is outside the live records from 8192"
        )]
        fn panics_on_a_release_before_the_tail() {
            writer(2, 3).release(4096);
        }
    }

    mod cursor {
        use super::*;

        /// The size of a record longer than one piece.
        const LONG: usize = 28 * ALIGN;

        /// A ring that holds the long record and its restart record.
        fn long_layout() -> Layout {
            Layout::new(to_u64(4 * LONG), LONG - HEADER_LEN)
                .expect("the long sizes make a ring")
        }

        /// The area of [`long_layout`] with one long data record at its start.
        fn long_area() -> Vec<u8> {
            let mut area = vec![0; 4 * LONG];
            let body = vec![7; LONG - HEADER_LEN];
            put(&mut area, 0, START.chain, Kind::Data.byte(), &body);
            area
        }

        /// The windows that read the long record from its start: block and blocks.
        const LONG_WINDOWS: [(usize, usize); 4] = [(0, 1), (1, 12), (13, 13), (26, 2)];

        fn put(
            area: &mut [u8],
            block: usize,
            chain: u32,
            kind: u8,
            body: &[u8],
        ) -> u32 {
            let (mut header, _) = record::header(chain, Kind::Data, []);
            let len = u32::try_from(body.len()).expect("a short body");
            header[..4].copy_from_slice(&len.to_le_bytes());
            header[8] = kind;
            let crc = [&header[..4], &[kind], body]
                .into_iter()
                .fold(chain, crate::crc32c::append);
            header[4..8].copy_from_slice(&crc.to_le_bytes());
            let at = block * ALIGN;
            area[at..at + HEADER_LEN].copy_from_slice(&header);
            area[at + HEADER_LEN..at + HEADER_LEN + body.len()].copy_from_slice(body);
            crc
        }

        #[test]
        fn finds_no_record_in_a_new_area() {
            let area = vec![0; index(AREA)];
            let (data, cursor) = walk(&area, START).expect("a zeroed area is valid");
            assert_eq!((data, cursor.at), (vec![], START));
        }

        #[test]
        fn starts_a_writer_with_a_restart_record_at_the_head() {
            let area = vec![0; index(AREA)];
            let (_, cursor) = walk(&area, START).expect("a zeroed area is valid");
            let (writer, sealed) = cursor.writer(0, 77).expect("the ring is empty");
            let body = 77u32.to_le_bytes();
            let (header, _) = record::header(START.chain, Kind::Restart, [&body[..]]);
            let record = Write { place: 0, header };
            assert_eq!(
                (writer.head(), sealed.wrap, sealed.record),
                (4096, None, record)
            );
        }

        #[test]
        #[should_panic(expected = "the chain ends at the last step")]
        fn panics_on_a_writer_before_the_end() {
            let _writer = Cursor::new(layout(), START, PIECE).writer(0, 1);
        }

        #[test]
        fn refuses_a_writer_when_the_ring_is_full_until_the_tail_moves() {
            let mut ring = Ring::new();
            for _ in 1..BLOCKS {
                ring.append(b"a").expect("the ring has room");
            }
            let (_, cursor) = walk(&ring.area, START).expect("a valid ring");
            let full = Full {
                needed: 4096,
                free: 0,
            };
            assert_eq!(cursor.writer(0, 1).map(drop), Err(full));
            let (_, cursor) = walk(&ring.area, START).expect("a valid ring");
            let (writer, sealed) = cursor.writer(4096, 1).expect("one block is free");
            assert_eq!((sealed.record.place, writer.head()), (0, 17 * 4096));
        }

        /// Each open writes its restart record right after the last data record,
        /// over the restart record of the open before it.
        #[test]
        fn starts_a_writer_after_the_last_data_record() {
            let mut ring = Ring::new();
            ring.append(b"one").expect("the ring has room");
            for chain in [5, 6] {
                assert_eq!(ring.reopen(chain), Ok(vec![b"one".to_vec()]));
                assert_eq!(ring.writer.head(), 3 * 4096, "the open of {chain}");
            }
            let (data, cursor) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!((data, cursor.at), (vec![b"one".to_vec()], at(3, 6)));
        }

        /// A trim after a reopen moves the tail to the end of a record that the
        /// walk read, with the chain value that the records after it continue.
        #[test]
        fn gives_the_writer_the_end_of_each_record() {
            let mut ring = Ring::new();
            for body in [&b"one"[..], b"two", &[7; BODY_MAX]] {
                ring.append(body).expect("the ring has room");
            }
            let three = ring.live.back().expect("three records").clone();
            let end = ring.head;
            ring.reopen(5).expect("the restart record fits");
            assert_eq!(ring.trim(Some(three.offset)), Some(three.start));
            assert_eq!(ring.walk().0, [vec![7; BODY_MAX]]);
            assert_eq!(ring.trim(None), Some(end));
            assert_eq!(ring.walk().0, Vec::<Vec<u8>>::new());
        }

        /// A ring with the same records trims to the same tail after a reopen: the
        /// restart record of the first open goes, and no data record.
        #[test]
        fn gives_the_writer_the_end_of_an_earlier_restart_record() {
            let mut ring = Ring::with(wide());
            for _ in 0..19 {
                ring.append(b"a").expect("the ring has room");
            }
            assert_eq!(ring.trim(None), None);
            ring.reopen(5).expect("the restart record fits");
            assert_eq!(ring.trim(None), Some(at(1, 1)));
            assert_eq!(ring.walk().0.len(), 19);
        }

        /// The restart record of an open goes over the records after the last data
        /// record, and the boundary after that data record stays.
        #[test]
        fn gives_the_writer_the_end_of_the_last_data_record() {
            let mut ring = Ring::new();
            ring.append(b"a").expect("the ring has room");
            ring.append(&[7; BODY_MAX]).expect("the ring has room");
            let end = ring.head;
            ring.reopen(5).expect("the restart record fits");
            assert_eq!(end.offset, 6 * 4096);
            assert_eq!(ring.trim(None), Some(end));
        }

        /// The writer starts before the restart records after the last data record,
        /// so a tail past the last data record is outside its records.
        #[test]
        #[should_panic(
            expected = "release to 4096 is outside the live records from 0 to 0"
        )]
        fn panics_on_a_tail_past_the_last_data_record() {
            let (_, cursor) = Ring::new().walk();
            let _writer = cursor.writer(4096, 2);
        }

        /// The smallest ring holds one restart record after any number of opens
        /// with no data, so it takes its largest record.
        #[test]
        fn takes_the_largest_record_after_opens_with_no_data() {
            let layout = Layout::new(4 * 4096, 4087).expect("an area of four blocks");
            let mut area = vec![0; 4 * 4096];
            for chain in 1..=5 {
                let mut cursor = Cursor::new(layout, START, PIECE);
                loop {
                    let Window { place, len } = cursor.window();
                    match cursor.next(&area[index(place)..index(place) + len]) {
                        Ok(Step::Moved) => {}
                        Ok(Step::End) => break,
                        other => panic!("the open of {chain} read {other:?}"),
                    }
                }
                let (mut writer, sealed) = cursor.writer(0, chain).expect("it fits");
                let place = index(sealed.record.place);
                area[place..place + HEADER_LEN].copy_from_slice(&sealed.record.header);
                area[place + HEADER_LEN..place + HEADER_LEN + RESTART_LEN]
                    .copy_from_slice(&chain.to_le_bytes());
                let appended = writer.append(4087).map(drop);
                assert_eq!(appended, Ok(()), "the open of {chain}");
            }
        }

        /// The log is cut at a damaged record and reopened. A new record with the
        /// bytes of the old one goes to the same place, before an old record.
        #[test]
        fn drops_an_old_record_after_a_cut_and_a_reopen() {
            let mut ring = Ring::new();
            for body in [&b"one"[..], b"two", b"three", b"four"] {
                ring.append(body).expect("the ring has room");
            }
            ring.area[2 * ALIGN + HEADER_LEN] ^= 1;
            ring.live.truncate(2);
            assert_eq!(ring.reopen(0xABCD), Ok(vec![b"one".to_vec()]));
            let plan = ring.append(b"three").expect("the ring has room");
            assert_eq!(plan.place, 3 * 4096);
            let (data, _) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!(data, [&b"one"[..], b"three"]);
        }

        #[test]
        fn asks_for_no_bytes_past_one_lap() {
            let mut ring = Ring::new();
            for _ in 2..BLOCKS {
                ring.append(b"a").expect("the ring has room");
            }
            let mut cursor = Cursor::new(layout(), START, PIECE);
            for _ in 1..BLOCKS {
                let Window { place, len } = cursor.window();
                let step = cursor.next(&ring.area[index(place)..index(place) + len]);
                assert!(matches!(step, Ok(Step::Data(_) | Step::Moved)), "{step:?}");
            }
            let last = Window {
                place: 15 * 4096,
                len: 4096,
            };
            assert_eq!(cursor.window(), last);
            ring.append(b"a").expect("the ring has room");
            assert_eq!(cursor.next(&ring.area[15 * ALIGN..]), Ok(data(b"a")));
            assert_eq!(cursor.window(), Window { place: 0, len: 0 });
            assert_eq!(cursor.next(&[]), Ok(Step::End));
        }

        #[test]
        fn reports_a_record_of_a_kind_it_does_not_know() {
            let mut area = vec![0; index(AREA)];
            put(&mut area, 0, START.chain, 4, b"");
            let invalid = Invalid { offset: 0, kind: 4 };
            assert_eq!(walk(&area, START).map(drop), Err(invalid));
        }

        #[test]
        fn reports_a_wrap_record_with_a_body() {
            let mut area = vec![0; index(AREA)];
            put(&mut area, 0, START.chain, Kind::Wrap.byte(), b"a");
            let invalid = Invalid { offset: 0, kind: 2 };
            assert_eq!(walk(&area, START).map(drop), Err(invalid));
        }

        #[test]
        fn reports_a_restart_record_without_a_chain_value() {
            let mut area = vec![0; index(AREA)];
            put(&mut area, 0, START.chain, Kind::Restart.byte(), b"abc");
            let invalid = Invalid { offset: 0, kind: 3 };
            assert_eq!(walk(&area, START).map(drop), Err(invalid));
        }

        #[test]
        fn opens_an_empty_ring_from_any_header_that_decodes() {
            use crate::header::Header;
            let tail = Position::new(u64::MAX - 4095, 7).expect("aligned");
            let block = Header::new(layout(), 7).next(tail).encode();
            let header = Header::decode(&block, &[0; ALIGN]).expect("a whole block");
            let area = vec![0; index(AREA)];
            let (data, cursor) =
                walk(&area, header.tail).expect("a zeroed area is valid");
            assert_eq!(data, Vec::<Vec<u8>>::new());
            let full = Full {
                needed: 4096,
                free: 4095,
            };
            assert_eq!(cursor.writer(header.tail.offset(), 1).map(drop), Err(full));
        }

        #[test]
        fn reports_a_record_that_passes_the_end_of_the_offsets() {
            let mut area = vec![0; index(AREA)];
            let chain = put(&mut area, 14, 5, Kind::Data.byte(), b"x");
            put(&mut area, 15, chain, Kind::Data.byte(), b"y");
            let tail = Position::new(u64::MAX - 8191, 5).expect("aligned");
            let invalid = Invalid {
                offset: u64::MAX - 4095,
                kind: 1,
            };
            assert_eq!(walk(&area, tail).map(drop), Err(invalid));
        }

        #[test]
        fn reports_a_wrap_record_at_the_tail_in_the_largest_area() {
            let area = u64::MAX - 12287;
            let layout = Layout::new(area, BODY_MAX).expect("the sizes make a ring");
            let tail = Position::new(area - 4096, 7).expect("aligned");
            let mut blocks = vec![0; 2 * ALIGN];
            let chain = put(&mut blocks, 0, 7, Kind::Data.byte(), b"x");
            put(&mut blocks, 1, chain, Kind::Wrap.byte(), &[]);
            let mut cursor = Cursor::new(layout, tail, PIECE);
            let first = Window {
                place: area - 4096,
                len: 4096,
            };
            assert_eq!(cursor.window(), first);
            assert_eq!(cursor.next(&blocks[..ALIGN]), Ok(data(b"x")));
            assert_eq!(
                cursor.window(),
                Window {
                    place: 0,
                    len: 4096
                }
            );
            let invalid = Invalid {
                offset: area,
                kind: 2,
            };
            assert_eq!(cursor.next(&blocks[ALIGN..]), Err(invalid));
        }

        /// No writer puts a wrap record where the skip ends at or after the tail's
        /// place one lap later: the record after it has no room.
        #[test]
        fn reports_a_wrap_record_that_reaches_the_tail() {
            let mut area = vec![0; index(AREA)];
            put(&mut area, 3, 5, Kind::Wrap.byte(), b"");
            let invalid = Invalid {
                offset: 3 * 4096,
                kind: 2,
            };
            assert_eq!(walk(&area, at(0, 5)).map(drop), Ok(()));
            assert_eq!(walk(&area, at(3, 5)).map(drop), Ok(()));
            let chain = put(&mut area, 0, 5, Kind::Data.byte(), &[0; 2 * ALIGN]);
            put(&mut area, 3, chain, Kind::Wrap.byte(), b"");
            assert_eq!(walk(&area, at(0, 5)).map(drop), Err(invalid));
        }

        #[test]
        #[should_panic(expected = "got 8192 bytes for a window of 4096 at 0")]
        fn panics_on_bytes_that_are_not_the_window() {
            let _step = Cursor::new(layout(), START, PIECE).next(&[0; 2 * ALIGN]);
        }

        /// A record within the table bound reads in two windows, its first block
        /// and the rest, then its start again, which is all of it.
        #[test]
        fn reads_a_record_within_the_table_bound_whole() {
            let mut ring = Ring::new();
            ring.append(&[7; BODY_MAX]).expect("the ring has room");
            let mut cursor = Cursor::new(layout(), START, PIECE);
            let window = |place: u64, len: usize| Window { place, len };
            assert_eq!(cursor.window(), window(0, ALIGN));
            assert_eq!(cursor.next(&ring.area[..ALIGN]), Ok(Step::Moved));
            assert_eq!(cursor.window(), window(4096, ALIGN));
            assert_eq!(cursor.next(&ring.area[ALIGN..2 * ALIGN]), Ok(Step::More));
            assert_eq!(cursor.window(), window(2 * 4096, 3 * ALIGN), "the rest");
            assert_eq!(
                cursor.next(&ring.area[2 * ALIGN..5 * ALIGN]),
                Ok(Step::More)
            );
            assert_eq!(cursor.window(), window(4096, 4 * ALIGN), "the start again");
            let step = cursor.next(&ring.area[ALIGN..5 * ALIGN]);
            assert_eq!(step, Ok(data(&[7; BODY_MAX])));
            assert_eq!(cursor.window(), window(5 * 4096, ALIGN));
            assert_eq!(cursor.next(&ring.area[5 * ALIGN..6 * ALIGN]), Ok(Step::End));
        }

        #[test]
        fn ends_at_a_record_whose_body_is_torn() {
            let mut ring = Ring::new();
            ring.append(&[7; 2 * ALIGN + 5]).expect("the ring has room");
            ring.area[3 * ALIGN] ^= 1;
            let mut cursor = Cursor::new(layout(), START, PIECE);
            assert_eq!(cursor.next(&ring.area[..ALIGN]), Ok(Step::Moved));
            assert_eq!(cursor.next(&ring.area[ALIGN..2 * ALIGN]), Ok(Step::More));
            assert_eq!(cursor.next(&ring.area[2 * ALIGN..4 * ALIGN]), Ok(Step::End));
            assert_eq!(cursor.offset(), 4096, "the walk stops at the torn record");
            let (_, cursor) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!(cursor.offset(), 4096);
        }

        /// The body pieces end at the table bound, then every piece; the start
        /// comes again up to the table bound.
        #[test]
        fn reads_a_long_record_in_pieces_then_its_start_again() {
            let area = long_area();
            let mut cursor = Cursor::new(long_layout(), START, PIECE);
            for (block, blocks) in LONG_WINDOWS {
                let window = Window {
                    place: to_u64(block * ALIGN),
                    len: blocks * ALIGN,
                };
                assert_eq!(cursor.window(), window, "block {block}");
                let piece = &area[block * ALIGN..(block + blocks) * ALIGN];
                assert_eq!(cursor.next(piece), Ok(Step::More), "block {block}");
            }
            assert_eq!(
                cursor.window(),
                Window {
                    place: 0,
                    len: TABLE
                }
            );
            let start = vec![7; TABLE - HEADER_LEN];
            let body = Body {
                start: &start,
                len: LONG - HEADER_LEN,
            };
            assert_eq!(cursor.next(&area[..TABLE]), Ok(Step::Data(body)));
            let next = Window {
                place: to_u64(LONG),
                len: ALIGN,
            };
            assert_eq!(cursor.window(), next);
            assert_eq!(cursor.next(&area[LONG..LONG + ALIGN]), Ok(Step::End));
        }

        #[test]
        fn ends_at_a_long_record_whose_body_is_torn() {
            let mut area = long_area();
            area[20 * ALIGN] ^= 1;
            let mut cursor = Cursor::new(long_layout(), START, PIECE);
            let steps = LONG_WINDOWS.map(|(block, blocks)| {
                cursor.next(&area[block * ALIGN..(block + blocks) * ALIGN])
            });
            let more = Ok(Step::More);
            assert_eq!(steps, [more, more, more, Ok(Step::End)]);
            assert_eq!(cursor.offset(), 0, "the walk stops at the torn record");
        }

        #[test]
        #[should_panic(
            expected = "the start of a checked record at 0 read the same twice"
        )]
        fn panics_when_the_start_of_a_record_reads_differently() {
            let area = long_area();
            let mut cursor = Cursor::new(long_layout(), START, PIECE);
            for (block, blocks) in LONG_WINDOWS {
                let step = cursor.next(&area[block * ALIGN..(block + blocks) * ALIGN]);
                assert_eq!(step, Ok(Step::More));
            }
            let mut start = area[..TABLE].to_vec();
            start[TABLE - 1] ^= 1;
            let _step = cursor.next(&start);
        }

        #[test]
        #[should_panic(expected = "a piece of 4097 bytes is not whole blocks")]
        fn panics_on_a_piece_that_is_not_whole_blocks() {
            let _cursor = Cursor::new(layout(), START, ALIGN + 1);
        }

        #[test]
        #[should_panic(
            expected = "a piece of 49152 bytes is not whole blocks of at least 53248"
        )]
        fn panics_on_a_piece_under_the_table_bound() {
            let _cursor = Cursor::new(layout(), START, TABLE - ALIGN);
        }

        /// A header that claims a size past the largest window, the end of the
        /// area, or one lap is no record, so the cursor does not read for it.
        #[test]
        fn ends_at_a_header_that_claims_too_much() {
            let mut area = vec![0; index(AREA)];
            let kind = Kind::Data.byte();
            put(&mut area, 0, START.chain, kind, &[1; 4 * ALIGN]);
            let mut cursor = Cursor::new(layout(), START, PIECE);
            assert_eq!(cursor.next(&area[..ALIGN]), Ok(Step::End));
            let last = at(BLOCKS - 1, 5);
            let (header, _) = record::header(5, Kind::Data, [[1; ALIGN].as_slice()]);
            area[15 * ALIGN..15 * ALIGN + HEADER_LEN].copy_from_slice(&header);
            let mut cursor = Cursor::new(layout(), last, PIECE);
            assert_eq!(cursor.next(&area[15 * ALIGN..]), Ok(Step::End));
            assert_eq!(cursor.window().len, 4096);
            let mut cursor = Cursor::new(layout(), at(0, 5), PIECE);
            cursor.at = last;
            assert_eq!(cursor.window().len, 4096);
            assert_eq!(cursor.next(&area[15 * ALIGN..]), Ok(Step::End));
        }

        /// A crash in the last write of a ring: the operations before it, its
        /// body, the sectors of the area that keep the write, a bit of the record
        /// to flip or not, the chain of the reopen, and the body after it.
        type Crash = (
            Vec<Op>,
            Vec<u8>,
            Vec<bool>,
            bool,
            prop::sample::Index,
            u32,
            Vec<u8>,
        );

        fn crash(layout: Layout) -> impl Strategy<Value = Crash> {
            let sectors = index(layout.area) / SECTOR;
            let kept = prop::collection::vec(prop::bool::weighted(0.9), sectors);
            let flip = any::<prop::sample::Index>();
            (
                ops(),
                body(),
                kept,
                any::<bool>(),
                flip,
                any::<u32>(),
                body(),
            )
        }

        /// A crash leaves any subset of the sectors of the last write. The record
        /// is live when every sector it wrote survives; lost padding does not
        /// count. A flipped bit in it drops it.
        fn keeps_the_last_record(
            layout: Layout,
            (ops, last, kept, damaged, flip, chain, after): Crash,
        ) -> Result<(), TestCaseError> {
            let opened = |op: &Op| matches!(op, Op::Reopen(c) if *c == chain);
            prop_assume!(chain != 1 && !ops.iter().any(opened));
            let mut ring = Ring::with(layout);
            run(&mut ring, &ops);
            let before = ring.area.clone();
            let Ok(plan) = ring.append(&last) else {
                return Err(TestCaseError::reject("the ring is full"));
            };
            let mut sectors = Vec::new();
            for (place, len) in plan
                .wrap
                .map(|place| (place, HEADER_LEN))
                .into_iter()
                .chain([(plan.place, HEADER_LEN + last.len())])
            {
                let first = index(place) / SECTOR;
                sectors.extend(first..=(index(place) + len - 1) / SECTOR);
            }
            for (sector, _) in kept.iter().enumerate().filter(|(_, kept)| !**kept) {
                let bytes = sector * SECTOR..(sector + 1) * SECTOR;
                ring.area[bytes.clone()].copy_from_slice(&before[bytes]);
            }
            let whole = sectors.iter().all(|sector| kept[*sector]);
            if whole && damaged {
                let written = HEADER_LEN + last.len();
                ring.area[index(plan.place) + flip.index(written)] ^= 1;
            }
            if !whole || damaged {
                ring.live.pop_back();
            }
            let mut expected = ring.data();
            let found = ring.reopen(chain);
            prop_assume!(found.is_ok());
            prop_assert_eq!(found, Ok(expected.clone()));
            prop_assume!(ring.append(&after).is_ok());
            expected.push(after);
            prop_assert_eq!(ring.walk().0, expected);
            Ok(())
        }

        proptest! {
            #[test]
            fn gives_the_live_data_in_order(ops in ops()) {
                for layout in [recorded(), layout()] {
                    let mut ring = Ring::with(layout);
                    run(&mut ring, &ops);
                    let (data, cursor) = ring.walk();
                    prop_assert_eq!(data, ring.data());
                    prop_assert_eq!(cursor.at, ring.head);
                    prop_assert_eq!(cursor.at.offset, ring.writer.head());
                }
            }

            /// A walk from the tail of any trim finds the records after it.
            #[test]
            fn walks_from_the_tail_of_a_trim(ops in trims(), wide in any::<bool>()) {
                let mut ring = Ring::with(if wide { self::wide() } else { layout() });
                run(&mut ring, &ops);
                let (data, cursor) = ring.walk();
                prop_assert_eq!(data, ring.data());
                prop_assert_eq!(cursor.at, ring.head);
            }

            #[test]
            fn keeps_a_last_record_only_when_its_sectors_survive(
                crash in crash(layout()),
            ) {
                keeps_the_last_record(layout(), crash)?;
            }

            #[test]
            fn keeps_a_last_record_of_the_recorded_ring_the_same(
                crash in crash(recorded()),
            ) {
                keeps_the_last_record(recorded(), crash)?;
            }

            /// `far` comes last, so the recorded cases give the values before it
            /// as they did.
            #[test]
            fn ends_within_one_lap_on_any_chained_records(
                records in prop::collection::vec((0..6u8, 0..6000usize), 0..12),
                tail in 0..recorded().area / 4096,
                chain in any::<u32>(),
                far in 0..BLOCKS,
            ) {
                for (layout, tail) in [(recorded(), tail), (layout(), far)] {
                    let blocks = index(layout.area) / ALIGN;
                    let mut area = vec![0xEE; index(layout.area)];
                    let (mut block, mut next) = (index(tail), chain);
                    for (kind, len) in &records {
                        let record = (HEADER_LEN + len).div_ceil(ALIGN);
                        if block + record > blocks {
                            block = 0;
                        }
                        next = put(&mut area, block, next, *kind, &vec![*kind; *len]);
                        block += record;
                    }
                    let tail = at(tail, chain);
                    let walked = match walk_in(layout, &area, tail) {
                        Ok((_, cursor)) => cursor.at.offset - tail.offset,
                        Err(invalid) => invalid.offset - tail.offset + 1,
                    };
                    prop_assert!(walked <= layout.area);
                }
            }
        }
    }
}
