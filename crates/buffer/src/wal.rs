//! Places the records of the write-ahead ring and walks them at recovery. It does no
//! I/O: the caller writes what a [`Writer`] plans and reads what a [`Cursor`] asks.
//!
//! The ring is an area of [`ALIGN`]-byte blocks, used in a circle. An offset counts
//! bytes since the ring was made and never wraps; its place in the area is the
//! offset modulo the area length. A record never crosses the end of the area: when
//! it does not fit in the rest, a wrap record goes there and the record goes to the
//! start. Each open of the ring walks it with a [`Cursor`], which then gives the
//! [`Writer`] and its restart record; the chain continues from the random value in
//! that record.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use crate::record::{self, ALIGN, HEADER_LEN, Kind};

fn to_u64(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length in memory fits in u64")
}

fn block() -> u64 {
    to_u64(ALIGN)
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
        if offset.is_multiple_of(block()) {
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

/// Sizes that do not make a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unfit {
    pub(crate) area: u64,
    pub(crate) body_max: usize,
}

/// The sizes of one ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
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
    /// under 4 or over `u32::MAX`, or when `area` is less than twice the largest
    /// record less one block. An empty ring of that length takes any record,
    /// wherever its head is.
    pub(crate) fn new(area: u64, body_max: usize) -> Result<Self, Unfit> {
        let window = HEADER_LEN
            .checked_add(body_max)
            .and_then(|len| len.checked_next_multiple_of(ALIGN))
            .map(to_u64);
        let restart =
            size_of::<u32>()..=usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        match window {
            Some(window)
                if restart.contains(&body_max)
                    && area.is_multiple_of(block())
                    && window.saturating_mul(2) - block() <= area =>
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
}

/// The ring has no room for a record. Space returns with [`Writer::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Full {
    /// Bytes of the area that the record needs, with the rest it must skip.
    pub(crate) needed: u64,
    /// Bytes of the area not in use.
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

/// The writes that put one record in the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// A wrap record to write first. It has no body.
    pub(crate) wrap: Option<Write>,
    pub(crate) record: Write,
    /// The boundary after the record.
    pub(crate) next: Position,
}

/// Plans where each record goes. It holds the live part of the ring: from the tail,
/// the oldest record still needed, to the head, where the next record starts.
/// [`Cursor::writer`] makes it.
#[derive(Debug)]
pub(crate) struct Writer {
    layout: Layout,
    tail: u64,
    head: Position,
}

impl Writer {
    /// Where the next record starts.
    pub(crate) fn head(&self) -> Position {
        self.head
    }

    /// Plans a data record with `body` as its bytes.
    ///
    /// # Errors
    ///
    /// [`Full`] when the record does not fit before the tail. Nothing changes.
    ///
    /// # Panics
    ///
    /// When `body` holds more bytes than the layout's maximum.
    pub(crate) fn append(&mut self, body: &[&[u8]]) -> Result<Plan, Full> {
        self.place(Kind::Data, body)
    }

    /// Frees the records before `tail`, a boundary that an earlier plan gave.
    ///
    /// # Panics
    ///
    /// When `tail` is outside the live part of the ring.
    pub(crate) fn release(&mut self, tail: Position) {
        assert!(
            (self.tail..=self.head.offset).contains(&tail.offset),
            "invariant: release to {} is outside the live records from {} to {}",
            tail.offset,
            self.tail,
            self.head.offset
        );
        self.tail = tail.offset;
    }

    fn place(&mut self, kind: Kind, body: &[&[u8]]) -> Result<Plan, Full> {
        let len = body.iter().map(|part| part.len()).sum::<usize>();
        assert!(
            len <= self.layout.body_max,
            "invariant: a body of {len} bytes is over the maximum of {}",
            self.layout.body_max
        );
        let size = to_u64((HEADER_LEN + len).next_multiple_of(ALIGN));
        let area = self.layout.area;
        let rest = area - self.head.offset % area;
        let skipped = if size > rest { rest } else { 0 };
        let free = area - (self.head.offset - self.tail);
        let needed = skipped + size;
        if needed > free {
            return Err(Full { needed, free });
        }
        let mut start = self.head;
        let wrap = (skipped > 0).then(|| {
            let (header, chain) = record::header(start.chain, Kind::Wrap, &[]);
            let place = start.offset % area;
            start = Position {
                offset: start.offset + skipped,
                chain,
            };
            Write { place, header }
        });
        let (header, chain) = record::header(start.chain, kind, body);
        self.head = Position {
            offset: start.offset + size,
            chain,
        };
        Ok(Plan {
            wrap,
            record: Write {
                place: start.offset % area,
                header,
            },
            next: self.head,
        })
    }
}

/// The bytes of the area that a [`Cursor`] reads next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) place: u64,
    pub(crate) len: u64,
}

/// One step of a [`Cursor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step<'a> {
    /// The body of the next data record.
    Data(&'a [u8]),
    /// A wrap or restart record. The cursor moved; ask for the next window.
    Moved,
    /// The chain ends here: this is the head of the ring.
    End,
}

/// A record that follows the chain but that this version cannot read: a kind it
/// does not know, or a wrap or restart record of the wrong shape. The ring is from
/// another version or a defect wrote it, so it must not be written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Invalid {
    pub(crate) offset: u64,
    pub(crate) kind: u8,
}

/// Walks the records of a ring from its tail at each open. Loop: read the bytes of
/// [`window`](Self::window) from the area, give them to [`next`](Self::next), and
/// stop at [`Step::End`]. Then [`writer`](Self::writer) continues the ring. It
/// ends within one lap of the area on any bytes.
#[derive(Debug)]
pub(crate) struct Cursor {
    layout: Layout,
    tail: u64,
    at: Position,
    ended: bool,
}

impl Cursor {
    /// Starts at `tail`, the boundary before the oldest live record. A ring that
    /// was just made has its tail at offset 0 with a random chain value.
    pub(crate) fn new(layout: Layout, tail: Position) -> Self {
        Self {
            layout,
            tail: tail.offset,
            at: tail,
            ended: false,
        }
    }

    pub(crate) fn window(&self) -> Window {
        let area = self.layout.area;
        let place = self.at.offset % area;
        let unread = area - (self.at.offset - self.tail);
        let len = self.layout.window.min(area - place).min(unread);
        Window { place, len }
    }

    /// Reads the record at the start of `bytes`, the bytes of the last
    /// [`window`](Self::window).
    ///
    /// # Errors
    ///
    /// [`Invalid`] for a record that follows the chain but cannot be read.
    ///
    /// # Panics
    ///
    /// When `bytes` is not the window.
    pub(crate) fn next<'a>(&mut self, bytes: &'a [u8]) -> Result<Step<'a>, Invalid> {
        let Window { place, len } = self.window();
        assert!(
            to_u64(bytes.len()) == len,
            "invariant: got {} bytes for a window of {len} at {place}",
            bytes.len()
        );
        let Some(record) = record::read(bytes, self.at.chain) else {
            self.ended = true;
            return Ok(Step::End);
        };
        let offset = self.at.offset;
        let read = offset - self.tail;
        let rest = self.layout.area - place;
        let (moved, chain, step) = match (Kind::decode(record.kind), record.body) {
            (Some(Kind::Data), body) => {
                (to_u64(record.size), record.crc, Step::Data(body))
            }
            (Some(Kind::Wrap), []) if read + rest < self.layout.area => {
                (rest, record.crc, Step::Moved)
            }
            (Some(Kind::Restart), &[c0, c1, c2, c3]) => {
                let chain = u32::from_le_bytes([c0, c1, c2, c3]);
                (to_u64(record.size), chain, Step::Moved)
            }
            _ => {
                let kind = record.kind;
                return Err(Invalid { offset, kind });
            }
        };
        self.at = Position {
            offset: offset + moved,
            chain,
        };
        Ok(step)
    }

    /// Makes the writer that continues the ring from the head, with the records
    /// before `tail` released, and plans its restart record. The body of that
    /// record is `chain`, a new random value, in little-endian bytes.
    ///
    /// # Errors
    ///
    /// [`Full`] when the restart record does not fit before `tail`. Move the
    /// records at the tail to a segment and call again with the later tail.
    ///
    /// # Panics
    ///
    /// Before [`Step::End`], or when `tail` is outside the records walked.
    pub(crate) fn writer(
        &self,
        tail: Position,
        chain: u32,
    ) -> Result<(Writer, Plan), Full> {
        assert!(
            self.ended,
            "invariant: a writer starts at the end of the chain"
        );
        let mut writer = Writer {
            layout: self.layout,
            tail: self.tail,
            head: self.at,
        };
        writer.release(tail);
        let mut plan = writer.place(Kind::Restart, &[&chain.to_le_bytes()])?;
        plan.next.chain = chain;
        writer.head = plan.next;
        Ok((writer, plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    const BLOCKS: u64 = 8;
    const AREA: u64 = BLOCKS * 4096;
    const BODY_MAX: usize = 3 * ALIGN;

    const START: Position = Position {
        offset: 0,
        chain: 0x5EED_0001,
    };

    fn layout() -> Layout {
        Layout::new(AREA, BODY_MAX).expect("the test sizes make a ring")
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    fn at(blocks: u64, chain: u32) -> Position {
        Position::new(blocks * 4096, chain).expect("a block count is aligned")
    }

    /// Walks the area with the real cursor to the end of the chain.
    fn walk(area: &[u8], tail: Position) -> Result<(Vec<Vec<u8>>, Cursor), Invalid> {
        let mut cursor = Cursor::new(layout(), tail);
        let mut data = Vec::new();
        for _ in 0..=2 * BLOCKS {
            let Window { place, len } = cursor.window();
            match cursor.next(&area[index(place)..index(place + len)])? {
                Step::Data(body) => data.push(body.to_vec()),
                Step::Moved => {}
                Step::End => return Ok((data, cursor)),
            }
        }
        panic!("the cursor did not end within two steps per block");
    }

    /// A live record of the model: the boundary before it and its data body.
    #[derive(Clone, Debug)]
    struct Live {
        start: Position,
        data: Option<Vec<u8>>,
    }

    /// An area in memory, the real writer, and a model of what is live.
    #[derive(Debug)]
    struct Ring {
        area: Vec<u8>,
        writer: Writer,
        tail: Position,
        live: VecDeque<Live>,
    }

    impl Ring {
        /// A ring that was just made and opened: it holds one restart record.
        fn new() -> Self {
            let area = vec![0; index(AREA)];
            let (_, cursor) = walk(&area, START).expect("a zeroed area is valid");
            let (writer, plan) = cursor.writer(START, 1).expect("the ring is empty");
            let mut ring = Self {
                area,
                writer,
                tail: START,
                live: VecDeque::new(),
            };
            ring.apply(START, &plan, &1u32.to_le_bytes(), false);
            ring
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

        fn apply(&mut self, start: Position, plan: &Plan, body: &[u8], data: bool) {
            let used = start.offset - self.tail.offset;
            if let Some(wrap) = plan.wrap {
                self.write(wrap.place, &wrap.header, used);
            }
            let mut bytes = plan.record.header.to_vec();
            bytes.extend_from_slice(body);
            self.write(plan.record.place, &bytes, used);
            let data = data.then(|| body.to_vec());
            self.live.push_back(Live { start, data });
        }

        fn append(&mut self, body: &[u8]) -> Result<Plan, Full> {
            let start = self.writer.head();
            let plan = self.writer.append(&[body])?;
            self.apply(start, &plan, body, true);
            Ok(plan)
        }

        fn release(&mut self, count: usize) {
            self.live.drain(..count.min(self.live.len()));
            let head = self.writer.head();
            self.tail = self.live.front().map_or(head, |live| live.start);
            self.writer.release(self.tail);
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
            let (writer, plan) = cursor.writer(self.tail, chain)?;
            self.writer = writer;
            self.apply(cursor.at, &plan, &chain.to_le_bytes(), false);
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
            let free = AREA - (head.offset - ring.tail.offset);
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
                ("a body under a restart value", 8 * block, 3),
                ("an area under two records less a block", 2 * block, 4088),
                ("a body over u32::MAX", u64::MAX - 4095, usize::MAX),
                ("a record size over u64", u64::MAX - 4095, usize::MAX - 8),
            ];
            for (case, area, body_max) in cases {
                let unfit = Unfit { area, body_max };
                assert_eq!(Layout::new(area, body_max), Err(unfit), "{case}");
            }
        }

        #[test]
        fn takes_an_area_of_two_records_less_a_block() {
            assert_eq!(
                Layout::new(4096, 4087).map(|layout| layout.window),
                Ok(4096)
            );
            assert_eq!(Layout::new(3 * 4096, 4088).map(|l| l.window), Ok(8192));
        }

        proptest! {
            #[test]
            fn lets_an_empty_ring_take_the_largest_record_at_any_head(
                blocks in 1..4u64,
                head in 0..64u64,
            ) {
                let body_max = index(blocks * 4096) - HEADER_LEN;
                let area = (2 * blocks - 1) * 4096;
                let layout = Layout::new(area, body_max).expect("the smallest area");
                let mut cursor = Cursor::new(layout, at(head, 0));
                let zeros = vec![0; index(cursor.window().len)];
                prop_assert_eq!(cursor.next(&zeros), Ok(Step::End));
                let (mut writer, restart) =
                    cursor.writer(at(head, 0), 1).expect("the ring is empty");
                writer.release(restart.next);
                let body = vec![0; body_max];
                prop_assert_eq!(writer.append(&[&body]).map(drop), Ok(()));
            }
        }
    }

    mod writer {
        use super::*;

        /// A writer with its tail and head at block counts, after its restart
        /// record of one block.
        fn writer(tail: u64, head: u64) -> Writer {
            let mut cursor = Cursor::new(layout(), at(head - 1, 9));
            let zeros = vec![0; index(cursor.window().len)];
            assert_eq!(cursor.next(&zeros), Ok(Step::End));
            let mut cursor = Cursor {
                tail: tail * 4096,
                ..cursor
            };
            cursor.ended = true;
            cursor.writer(at(tail, 9), 9).expect("the ring has room").0
        }

        #[test]
        fn places_records_back_to_back_from_the_head() {
            let mut writer = writer(0, 1);
            let first = writer.append(&[b"a"]).expect("the ring has room");
            let second = writer.append(&[&[7; ALIGN]]).expect("the ring has room");
            assert_eq!((first.wrap, first.record.place), (None, 4096));
            assert_eq!(first.next.offset, 2 * 4096);
            assert_eq!((second.wrap, second.record.place), (None, 2 * 4096));
            assert_eq!(second.next.offset, 4 * 4096);
            assert_eq!(writer.head(), second.next);
        }

        #[test]
        fn fills_a_block_with_a_body_of_4087_bytes() {
            let mut writer = writer(0, 1);
            let fits = writer.append(&[&[7; 4087]]).expect("the ring has room");
            let spills = writer.append(&[&[7; 4088]]).expect("the ring has room");
            assert_eq!(fits.next.offset, 2 * 4096);
            assert_eq!(spills.next.offset, 4 * 4096);
        }

        #[test]
        fn puts_a_wrap_record_when_a_record_does_not_fit_in_the_rest() {
            let mut writer = writer(6, 7);
            let plan = writer.append(&[&[7; ALIGN]]).expect("the ring has room");
            assert_eq!(plan.wrap.map(|wrap| wrap.place), Some(7 * 4096));
            assert_eq!((plan.record.place, plan.next.offset), (0, 10 * 4096));
        }

        #[test]
        fn refuses_a_record_that_does_not_fit_before_the_tail() {
            let mut writer = writer(1, 7);
            let head = writer.head();
            let full = Full {
                needed: 3 * 4096,
                free: 2 * 4096,
            };
            assert_eq!(writer.append(&[&[7; ALIGN]]), Err(full));
            assert_eq!(writer.head(), head);
            writer.release(at(2, 0));
            let plan = writer.append(&[&[7; ALIGN]]);
            assert_eq!(plan.map(|plan| plan.record.place), Ok(0));
        }

        #[test]
        #[should_panic(expected = "a body of 12289 bytes is over the maximum of 12288")]
        fn panics_on_a_body_over_the_maximum() {
            let _plan = writer(0, 1).append(&[&[0; BODY_MAX + 1]]);
        }

        #[test]
        #[should_panic(expected = "release to 8192 is outside the live records from 0")]
        fn panics_on_a_release_past_the_head() {
            writer(0, 1).release(at(2, 0));
        }

        #[test]
        #[should_panic(
            expected = "release to 4096 is outside the live records from 8192"
        )]
        fn panics_on_a_release_before_the_tail() {
            writer(2, 3).release(at(1, 0));
        }
    }

    mod cursor {
        use super::*;

        fn put(
            area: &mut [u8],
            block: usize,
            chain: u32,
            kind: u8,
            body: &[u8],
        ) -> u32 {
            let (mut header, _) = record::header(chain, Kind::Data, &[]);
            let len = u32::try_from(body.len()).expect("a short body");
            header[..4].copy_from_slice(&len.to_le_bytes());
            header[8] = kind;
            let mut crc = crate::crc32c::Crc32c::resume(chain);
            crc.update(&header[..4]);
            crc.update(&[kind]);
            crc.update(body);
            let crc = crc.finish();
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
            let (writer, plan) = cursor.writer(START, 77).expect("the ring is empty");
            let (header, _) =
                record::header(START.chain, Kind::Restart, &[&77u32.to_le_bytes()]);
            let record = Write { place: 0, header };
            assert_eq!((plan.wrap, plan.record), (None, record));
            assert_eq!((plan.next, writer.head()), (at(1, 77), at(1, 77)));
        }

        #[test]
        #[should_panic(expected = "a writer starts at the end of the chain")]
        fn panics_on_a_writer_before_the_end() {
            let _writer = Cursor::new(layout(), START).writer(START, 1);
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
            assert_eq!(cursor.writer(START, 5).map(drop), Err(full));
            let (writer, plan) = cursor.writer(at(1, 1), 5).expect("one block is free");
            assert_eq!((plan.record.place, writer.head()), (0, at(9, 5)));
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
            assert_eq!(plan.record.place, 3 * 4096);
            let (data, _) = walk(&ring.area, START).expect("a valid ring");
            assert_eq!(data, [&b"one"[..], b"three"]);
        }

        #[test]
        fn asks_for_no_bytes_past_one_lap() {
            let mut ring = Ring::new();
            for _ in 2..BLOCKS {
                ring.append(b"a").expect("the ring has room");
            }
            let mut cursor = Cursor::new(layout(), START);
            for _ in 1..BLOCKS {
                let Window { place, len } = cursor.window();
                let step = cursor.next(&ring.area[index(place)..index(place + len)]);
                assert!(matches!(step, Ok(Step::Data(_) | Step::Moved)), "{step:?}");
            }
            let last = Window {
                place: 7 * 4096,
                len: 4096,
            };
            assert_eq!(cursor.window(), last);
            ring.append(b"a").expect("the ring has room");
            assert_eq!(cursor.next(&ring.area[7 * ALIGN..]), Ok(Step::Data(b"a")));
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
        #[should_panic(expected = "got 4096 bytes for a window of 16384 at 0")]
        fn panics_on_bytes_that_are_not_the_window() {
            let _step = Cursor::new(layout(), START).next(&[0; ALIGN]);
        }

        proptest! {
            #[test]
            fn gives_the_live_data_in_order(ops in ops()) {
                let mut ring = Ring::new();
                run(&mut ring, &ops);
                let (data, cursor) = walk(&ring.area, ring.tail).expect("a valid ring");
                prop_assert_eq!(data, ring.data());
                prop_assert_eq!(cursor.at, ring.writer.head());
            }

            #[test]
            fn drops_a_last_record_that_is_torn_or_damaged(
                ops in ops(),
                last in body(),
                kept in any::<[bool; 8]>(),
                flip in any::<prop::sample::Index>(),
                after in body(),
            ) {
                let mut ring = Ring::new();
                run(&mut ring, &ops);
                let before = ring.area.clone();
                let mut expected = ring.data();
                let Ok(plan) = ring.append(&last) else {
                    return Err(TestCaseError::reject("the ring is full"));
                };
                ring.live.pop_back();
                let whole = ring.area.clone();
                for (block, _) in kept.iter().enumerate().filter(|(_, kept)| !**kept) {
                    let block = block * ALIGN..(block + 1) * ALIGN;
                    ring.area[block.clone()].copy_from_slice(&before[block]);
                }
                if ring.area == whole {
                    let written = HEADER_LEN + last.len();
                    ring.area[index(plan.record.place) + flip.index(written)] ^= 1;
                }
                let found = ring.reopen(1);
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
                    Ok((_, cursor)) => prop_assert!(cursor.at.offset - tail.offset <= AREA),
                    Err(invalid) => prop_assert!(invalid.offset - tail.offset < AREA),
                }
            }
        }
    }
}
