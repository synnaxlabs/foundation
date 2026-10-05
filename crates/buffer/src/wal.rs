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
//! The writer places records by their length and keeps no chain. [`Plan::seal`]
//! makes the headers of a placed record from the chain value before it, so the
//! CRC over the body runs where the caller seals, in record order.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use crate::entry;
use crate::record::{
    self, ALIGN, AREA_START, BLOCK, Body, Check, HEADER_LEN, Head, Kind, Record,
};

/// The body of a restart record: one chain value.
const RESTART_LEN: usize = 4;

/// Bytes of the whole blocks that hold a record header and the largest entry table.
const TABLE: usize = (HEADER_LEN + entry::TABLE_MAX).next_multiple_of(ALIGN);

fn to_u64(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length in memory fits in u64")
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
    /// A boundary read from the ring header.
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
    /// A ring of `area` bytes whose records hold a body of at most `body_max` bytes.
    ///
    /// # Errors
    ///
    /// [`Unfit`] when `area` is not a multiple of [`ALIGN`], when `body_max` is
    /// under the table of one entry or over `u32::MAX`, when `area` is less than
    /// twice the largest record, or when the ring file (two header blocks and the
    /// area) does not fit in a `u64`. A ring of that length that holds only its
    /// restart record takes any record, wherever the restart record is.
    pub fn new(area: u64, body_max: usize) -> Result<Self, Unfit> {
        let window = HEADER_LEN
            .checked_add(body_max)
            .and_then(|len| len.checked_next_multiple_of(ALIGN))
            .map(to_u64);
        let body =
            entry::table_len(1)..=usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        match window {
            Some(window)
                if body.contains(&body_max)
                    && area.is_multiple_of(BLOCK)
                    && area <= u64::MAX - AREA_START
                    && window.saturating_mul(2) <= area =>
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

    /// The area in bytes.
    #[must_use]
    pub fn area(self) -> u64 {
        self.area
    }

    /// The most bytes one record body holds.
    #[must_use]
    pub fn body_max(self) -> usize {
        self.body_max
    }

    /// The length of the ring file: the two header blocks and the area.
    pub(crate) fn file_len(self) -> u64 {
        AREA_START + self.area
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

/// The places of one record in the ring. [`seal`](Self::seal) makes its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The place of a wrap record before the record, when the record wraps.
    pub(crate) wrap: Option<u64>,
    /// The place of the record.
    pub(crate) place: u64,
    /// The offset after the record.
    pub(crate) next: u64,
    /// The length of the body the record was placed for.
    len: usize,
}

impl Plan {
    /// Makes the headers of the data record, with `body` as its bytes, chained
    /// from `chain`, and returns them with the chain value of the next record.
    ///
    /// # Panics
    ///
    /// When `body` is not the length the record was placed for.
    pub(crate) fn seal<'a>(
        self,
        chain: u32,
        body: impl IntoIterator<Item = &'a [u8], IntoIter: Clone>,
    ) -> (Sealed, u32) {
        self.headers(chain, Kind::Data, body)
    }

    fn headers<'a>(
        self,
        chain: u32,
        kind: Kind,
        body: impl IntoIterator<Item = &'a [u8], IntoIter: Clone>,
    ) -> (Sealed, u32) {
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
            Write { place, header }
        });
        let (header, next) = record::header(chain, kind, body);
        let record = Write {
            place: self.place,
            header,
        };
        (Sealed { wrap, record }, next)
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
        let area = self.layout.area;
        let wrap = (skipped > 0).then(|| self.head % area);
        let start = self.head + skipped;
        self.head = start + size;
        Ok(Plan {
            wrap,
            place: start % area,
            next: self.head,
            len,
        })
    }

    /// The most bytes a record body holds.
    pub(crate) fn body_max(&self) -> usize {
        self.layout.body_max
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
        let rest = area - self.head % area;
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
    /// The chain ends here: this is the head of the ring.
    End,
}

/// A record that follows the chain but that this version cannot read: a kind it
/// does not know, a wrap or restart record of the wrong shape, or a record that
/// ends past the end of the offsets. The ring is from another version or a defect
/// wrote it, so it must not be written to.
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
/// stop at [`Step::End`]. Then [`writer`](Self::writer) continues the ring. It
/// ends within one lap of the area on any bytes.
///
/// A window is one block, or a piece of a record longer than one block, at most
/// `piece` bytes. The cursor reads such a record in three parts: its first block,
/// the rest of its body in pieces while the CRC runs, then its start again, up to
/// [`TABLE`] bytes, for the first bytes of the body. A walk reads at most twice
/// the live bytes, plus one block and one largest record for a torn record at the
/// end, and holds one window at a time.
#[derive(Debug)]
pub(crate) struct Cursor {
    layout: Layout,
    tail: u64,
    at: Position,
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

    /// The largest window at the head: the largest record, the rest of the area,
    /// or the rest of one lap.
    fn bound(&self) -> Window {
        let area = self.layout.area;
        let place = self.at.offset % area;
        let len = self.layout.window.min(area - place).min(self.unread());
        let len = usize::try_from(len).expect("invariant: a window fits in memory");
        Window { place, len }
    }

    /// Bytes of the area between the head and the tail one lap later.
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
    /// differently the second time: no writer runs during a walk, so the bytes
    /// the CRC covered must come back.
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
        let rest = self.layout.area - offset % self.layout.area;
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
        Ok(step)
    }

    /// Makes the writer that continues the ring from the head, with the records
    /// before the offset `tail` released, and seals its restart record with
    /// `chain`, a new random value, as the body. The chain continues from `chain`.
    ///
    /// # Errors
    ///
    /// [`Full`] when the restart record does not fit before `tail`. Move the
    /// records at the tail to a segment and call again with the later tail. No tail
    /// helps when the head is at the end of the offsets: the ring is full for good.
    ///
    /// # Panics
    ///
    /// Before [`Step::End`], or when `tail` is outside the records walked.
    pub(crate) fn writer(
        &self,
        tail: u64,
        chain: u32,
    ) -> Result<(Writer, Sealed), Full> {
        assert!(self.ended, "invariant: the chain ends at the last step");
        let mut writer = Writer {
            layout: self.layout,
            tail: self.tail,
            head: self.at.offset,
        };
        writer.release(tail);
        let plan = writer.append(RESTART_LEN)?;
        let body = chain.to_le_bytes();
        let (sealed, _) = plan.headers(self.at.chain, Kind::Restart, [&body[..]]);
        Ok((writer, sealed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use env::files::SECTOR;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    const BLOCKS: u64 = 8;
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

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    fn at(blocks: u64, chain: u32) -> Position {
        Position::new(blocks * 4096, chain).expect("a block count is aligned")
    }

    /// Walks the area with the real cursor to the end of the chain. Checks that it
    /// asks for at most twice the live bytes, one block, and one largest record.
    fn walk(area: &[u8], tail: Position) -> Result<(Vec<Vec<u8>>, Cursor), Invalid> {
        let mut cursor = Cursor::new(layout(), tail, PIECE);
        let mut data = Vec::new();
        let mut asked = 0;
        for _ in 0..=6 * BLOCKS {
            let Window { place, len } = cursor.window();
            asked += to_u64(len);
            match cursor.next(&area[index(place)..index(place) + len])? {
                Step::Data(body) => data.push(whole(area, place, body)),
                Step::Moved | Step::More => {}
                Step::End => {
                    let live = cursor.at.offset - tail.offset;
                    let window = (BODY_MAX + HEADER_LEN).next_multiple_of(ALIGN);
                    let most = 2 * live + to_u64(ALIGN + window);
                    assert!(asked <= most, "asked {asked} for {live} live bytes");
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
        data: Option<Vec<u8>>,
    }

    /// An area in memory, the real writer, and a model of what is live. The model
    /// keeps the chain at the head: the writer has none.
    #[derive(Debug)]
    struct Ring {
        area: Vec<u8>,
        writer: Writer,
        tail: Position,
        head: Position,
        live: VecDeque<Live>,
    }

    impl Ring {
        /// A ring that was just made and opened: it holds one restart record.
        fn new() -> Self {
            let area = vec![0; index(AREA)];
            let (_, cursor) = walk(&area, START).expect("a zeroed area is valid");
            let (writer, sealed) = cursor.writer(0, 1).expect("the ring is empty");
            let mut ring = Self {
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
            assert_eq!(place % 4096, 0, "a write starts off a block boundary");
            assert!(place + len <= AREA, "a write runs past the area");
            let first = self.tail.offset % AREA;
            let to_end = used.min(AREA - first);
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
            let data = data.then(|| body.to_vec());
            self.live.push_back(Live { start, data });
        }

        fn append(&mut self, body: &[u8]) -> Result<Plan, Full> {
            let plan = self.writer.append(body.len())?;
            let (sealed, chain) = plan.seal(self.head.chain, [body]);
            self.apply(&sealed, body, true);
            self.head = Position {
                offset: plan.next,
                chain,
            };
            Ok(plan)
        }

        fn release(&mut self, count: usize) {
            self.live.drain(..count.min(self.live.len()));
            self.tail = self.live.front().map_or(self.head, |live| live.start);
            self.writer.release(self.tail.offset);
        }

        fn data(&self) -> Vec<Vec<u8>> {
            let data = self.live.iter();
            data.filter_map(|live| live.data.clone()).collect()
        }

        fn walk(&self) -> (Vec<Vec<u8>>, Cursor) {
            walk(&self.area, self.tail).expect("a valid ring")
        }

        /// Drops the writer, as a crash does, walks the area, and continues with a
        /// new writer. Gives the data that the walk found.
        fn reopen(&mut self, chain: u32) -> Result<Vec<Vec<u8>>, Full> {
            let (data, cursor) = self.walk();
            let (writer, sealed) = cursor.writer(self.tail.offset, chain)?;
            self.writer = writer;
            self.head = cursor.at;
            self.restart(&sealed, chain);
            Ok(data)
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Append(Vec<u8>),
        Reopen(u32),
        Release(usize),
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

    /// Runs `ops` and checks each refusal and each reopen against the model.
    fn run(ring: &mut Ring, ops: &[Op]) {
        for op in ops {
            let head = ring.writer.head();
            let free = AREA - (head - ring.tail.offset);
            let live = ring.data();
            let result = match op {
                Op::Append(body) => ring.append(body).map(drop),
                Op::Reopen(chain) => ring.reopen(*chain).map(|data| {
                    assert_eq!(data, live, "data found at a reopen");
                }),
                Op::Release(count) => {
                    ring.release(*count);
                    Ok(())
                }
            };
            if let Err(full) = result {
                assert_eq!(full.free, free, "free bytes at {op:?}");
                assert!(full.needed > free, "a record that fits was refused");
                assert_eq!(ring.writer.head(), head, "a refusal moved the head");
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
                (
                    "a body under one entry table",
                    8 * block,
                    entry::table_len(1) - 1,
                ),
                ("an area under two records less a block", 2 * block, 4088),
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
        fn takes_a_body_of_one_entry_table() {
            let window = Layout::new(2 * 4096, entry::table_len(1)).map(|l| l.window);
            assert_eq!(window, Ok(4096));
        }

        #[test]
        fn takes_an_area_of_two_records() {
            assert_eq!(
                Layout::new(2 * 4096, 4087).map(|layout| layout.window),
                Ok(4096)
            );
            assert_eq!(Layout::new(4 * 4096, 4088).map(|l| l.window), Ok(8192));
        }

        proptest! {
            /// The smallest area is twice the largest record. A ring of that area
            /// that holds only its restart record takes its largest record,
            /// wherever the restart record is.
            #[test]
            fn takes_the_largest_record_after_the_restart_record_at_any_tail(
                body_max in entry::table_len(1)..=3 * ALIGN - HEADER_LEN,
                tail in 0..64u64,
            ) {
                let window = to_u64((HEADER_LEN + body_max).next_multiple_of(ALIGN));
                let under = 2 * window - 4096;
                let unfit = Unfit { area: under, body_max };
                prop_assert_eq!(Layout::new(under, body_max), Err(unfit));
                let layout =
                    Layout::new(2 * window, body_max).expect("the smallest area");
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
            let plan = |wrap, place, next, len| Plan {
                wrap,
                place,
                next,
                len,
            };
            assert_eq!(first, plan(None, 4096, 2 * 4096, 1));
            assert_eq!(second, plan(None, 2 * 4096, 4 * 4096, ALIGN));
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
            let mut writer = writer(6, 7);
            let plan = writer.append(ALIGN).expect("the ring has room");
            assert_eq!(
                (plan.wrap, plan.place, plan.next),
                (Some(7 * 4096), 0, 10 * 4096)
            );
            let (sealed, chain) = plan.seal(9, [[7; ALIGN].as_slice()]);
            let (wrap, after_wrap) = record::header(9, Kind::Wrap, []);
            let (header, after) =
                record::header(after_wrap, Kind::Data, [[7; ALIGN].as_slice()]);
            let expected = Sealed {
                wrap: Some(Write {
                    place: 7 * 4096,
                    header: wrap,
                }),
                record: Write { place: 0, header },
            };
            assert_eq!((sealed, chain), (expected, after));
        }

        /// Two records placed before either is sealed read back when each is
        /// sealed from the chain value of the one before, and the second does
        /// not when it is sealed from the first's chain.
        #[test]
        fn seal_chains_each_record_from_the_one_before() {
            let mut ring = Ring::new();
            let first = ring.writer.append(1).expect("the ring has room");
            let second = ring.writer.append(1).expect("the ring has room");
            let (a, chain) = first.seal(ring.head.chain, [b"a".as_slice()]);
            let (b, _) = second.seal(chain, [b"b".as_slice()]);
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
            let small = Layout::new(5 * 4096, 4088).expect("the sizes make a ring");
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
            let mut writer = opened(layout(), u64::MAX - 40959);
            let plan = writer.append(ALIGN).expect("fits");
            assert_eq!(plan.wrap, Some(28672));
            assert_eq!(plan.place, 0);
            assert_eq!(plan.next, u64::MAX - 24575);
        }

        #[test]
        fn refuses_a_record_that_does_not_fit_before_the_tail() {
            let mut writer = writer(1, 7);
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
            Layout::new(to_u64(2 * LONG), LONG - HEADER_LEN)
                .expect("the long sizes make a ring")
        }

        /// The area of [`long_layout`] with one long data record at its start.
        fn long_area() -> Vec<u8> {
            let mut area = vec![0; 2 * LONG];
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
            let (writer, sealed) = cursor.writer(4096, 1).expect("one block is free");
            assert_eq!((sealed.record.place, writer.head()), (0, 9 * 4096));
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
                place: 7 * 4096,
                len: 4096,
            };
            assert_eq!(cursor.window(), last);
            ring.append(b"a").expect("the ring has room");
            assert_eq!(cursor.next(&ring.area[7 * ALIGN..]), Ok(data(b"a")));
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
            let chain = put(&mut area, 6, 5, Kind::Data.byte(), b"x");
            put(&mut area, 7, chain, Kind::Data.byte(), b"y");
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
            assert_eq!(cursor.offset(), 4096, "the head is at the torn record");
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
            assert_eq!(cursor.offset(), 0, "the head is at the torn record");
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
            area[7 * ALIGN..7 * ALIGN + HEADER_LEN].copy_from_slice(&header);
            let mut cursor = Cursor::new(layout(), last, PIECE);
            assert_eq!(cursor.next(&area[7 * ALIGN..]), Ok(Step::End));
            assert_eq!(cursor.window().len, 4096);
            let mut cursor = Cursor::new(layout(), at(0, 5), PIECE);
            cursor.at = last;
            assert_eq!(cursor.window().len, 4096);
            assert_eq!(cursor.next(&area[7 * ALIGN..]), Ok(Step::End));
        }

        proptest! {
            #[test]
            fn gives_the_live_data_in_order(ops in ops()) {
                let mut ring = Ring::new();
                run(&mut ring, &ops);
                let (data, cursor) = walk(&ring.area, ring.tail).expect("a valid ring");
                prop_assert_eq!(data, ring.data());
                prop_assert_eq!(cursor.at, ring.head);
                prop_assert_eq!(cursor.at.offset, ring.writer.head());
            }

            /// A crash leaves any subset of the sectors of the last
            /// write. The record is live when every sector it wrote survives;
            /// lost padding does not count. A flipped bit in it drops it.
            #[test]
            fn keeps_a_last_record_only_when_its_sectors_survive(
                ops in ops(),
                last in body(),
                kept in prop::collection::vec(
                    prop::bool::weighted(0.9),
                    index(AREA) / SECTOR,
                ),
                damaged in any::<bool>(),
                flip in any::<prop::sample::Index>(),
                chain in any::<u32>(),
                after in body(),
            ) {
                let opened = |op: &Op| matches!(op, Op::Reopen(c) if *c == chain);
                prop_assume!(chain != 1 && !ops.iter().any(opened));
                let mut ring = Ring::new();
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
                let (data, _) = walk(&ring.area, ring.tail).expect("a valid ring");
                prop_assert_eq!(data, expected);
            }

            #[test]
            fn ends_within_one_lap_on_any_chained_records(
                records in prop::collection::vec((0..6u8, 0..6000usize), 0..12),
                tail in 0..BLOCKS,
                chain in any::<u32>(),
            ) {
                let mut area = vec![0xEE; index(AREA)];
                let (mut block, mut next) = (index(tail), chain);
                for (kind, len) in records {
                    let blocks = (HEADER_LEN + len).div_ceil(ALIGN);
                    if block + blocks > index(BLOCKS) {
                        block = 0;
                    }
                    next = put(&mut area, block, next, kind, &vec![kind; len]);
                    block += blocks;
                }
                let tail = at(tail, chain);
                match walk(&area, tail) {
                    Ok((_, cursor)) => {
                        prop_assert!(cursor.at.offset - tail.offset <= AREA);
                    }
                    Err(invalid) => prop_assert!(invalid.offset - tail.offset < AREA),
                }
            }
        }
    }
}
