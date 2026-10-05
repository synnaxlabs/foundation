//! Places the records of the write-ahead ring and walks them at recovery. It does no
//! I/O: the caller writes what a [`Writer`] plans and reads what a [`Cursor`] asks.
//!
//! The ring is an area of [`ALIGN`]-byte blocks, used in a circle. An offset counts
//! bytes since the ring was made and never wraps; its place in the area is the
//! offset modulo the area length. A record never crosses the end of the area: when
//! it does not fit in the rest, a wrap record goes there and the record goes to the
//! start. Each open of the ring writes a restart record, and the chain continues
//! from the random value in it.

use crate::record::{self, ALIGN, HEADER_LEN};

/// Bytes that a [`Writer`] makes for each record: the header and the kind.
pub(crate) const PREFIX_LEN: usize = HEADER_LEN + 1;

const ALIGN_U64: u64 = ALIGN as u64;

const DATA: u8 = 1;
const WRAP: u8 = 2;
const RESTART: u8 = 3;

/// A record boundary: where the next record starts and the chain value it follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) offset: u64,
    pub(crate) chain: u32,
}

/// The sizes of one ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    area: u64,
    body_max: usize,
}

impl Layout {
    /// A ring of `area` bytes whose records hold at most `body_max` bytes.
    ///
    /// # Panics
    ///
    /// When `area` is not a multiple of [`ALIGN`], or a record of `body_max`
    /// bytes does not fit in it.
    pub(crate) fn new(area: u64, body_max: usize) -> Self {
        let layout = Self { area, body_max };
        assert!(
            area.is_multiple_of(ALIGN_U64) && layout.window_max() <= area,
            "invariant: a ring of {area} bytes does not hold whole blocks and a \
             record of {body_max} bytes"
        );
        layout
    }

    /// The size of the largest record.
    fn window_max(self) -> u64 {
        size(PREFIX_LEN + self.body_max)
    }
}

/// The bytes that a record with `written` header, kind, and body bytes takes.
fn size(written: usize) -> u64 {
    u64::try_from(written.next_multiple_of(ALIGN))
        .expect("invariant: a record length fits in u64")
}

/// The ring has no room for a record. Space returns with [`Writer::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Full {
    /// Bytes of the area that the record needs, with the rest it must skip.
    pub(crate) needed: u64,
    /// Bytes of the area not in use.
    pub(crate) free: u64,
}

/// The writes that put one record in the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// A wrap record to write first: its place in the area and its bytes.
    pub(crate) wrap: Option<(u64, [u8; PREFIX_LEN])>,
    /// The place of the record in the area.
    pub(crate) place: u64,
    /// The bytes to write there, followed by the body of the record.
    pub(crate) prefix: [u8; PREFIX_LEN],
    /// The boundary after the record.
    pub(crate) next: Position,
}

/// Plans where each record goes. It holds the live part of the ring: from `tail`,
/// the oldest record still needed, to `head`, where the next record starts.
#[derive(Debug)]
pub(crate) struct Writer {
    layout: Layout,
    tail: u64,
    head: Position,
}

impl Writer {
    /// Continues a ring whose live records run from offset `tail` to `head`. A
    /// [`Cursor`] gives `head`.
    pub(crate) fn new(layout: Layout, tail: u64, head: Position) -> Self {
        let writer = Self { layout, tail, head };
        assert!(
            tail <= head.offset && writer.used() <= layout.area,
            "invariant: live records from {tail} to {} do not fit a ring of {}",
            head.offset,
            layout.area
        );
        writer
    }

    /// Where the next record starts.
    pub(crate) fn head(&self) -> Position {
        self.head
    }

    fn used(&self) -> u64 {
        self.head.offset - self.tail
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
        self.place(DATA, body)
    }

    /// Plans the restart record that starts a chain at `chain`. Its body is `chain`
    /// in little-endian bytes. Call it once at each open, before any append.
    ///
    /// # Errors
    ///
    /// [`Full`] when the record does not fit before the tail. Nothing changes.
    pub(crate) fn restart(&mut self, chain: u32) -> Result<Plan, Full> {
        let mut plan = self.place(RESTART, &[&chain.to_le_bytes()])?;
        plan.next.chain = chain;
        self.head = plan.next;
        Ok(plan)
    }

    /// Frees the records before offset `tail`, a boundary that an earlier plan gave.
    ///
    /// # Panics
    ///
    /// When `tail` is outside the live part of the ring.
    pub(crate) fn release(&mut self, tail: u64) {
        assert!(
            (self.tail..=self.head.offset).contains(&tail),
            "invariant: release to {tail} is outside the live records from {} to {}",
            self.tail,
            self.head.offset
        );
        self.tail = tail;
    }

    fn place(&mut self, kind: u8, body: &[&[u8]]) -> Result<Plan, Full> {
        let len = body.iter().map(|part| part.len()).sum::<usize>();
        assert!(
            len <= self.layout.body_max,
            "invariant: a record of {len} bytes is over the maximum of {}",
            self.layout.body_max
        );
        let size = size(PREFIX_LEN + len);
        let area = self.layout.area;
        let rest = area - self.head.offset % area;
        let skipped = if size > rest { rest } else { 0 };
        let free = area - self.used();
        let needed = skipped + size;
        if needed > free {
            return Err(Full { needed, free });
        }
        let mut start = self.head;
        let wrap = (skipped > 0).then(|| {
            let (prefix, chain) = prefix(start.chain, WRAP, &[]);
            let wrap = (start.offset % area, prefix);
            start = Position {
                offset: start.offset + skipped,
                chain,
            };
            wrap
        });
        let (prefix, chain) = prefix(start.chain, kind, body);
        self.head = Position {
            offset: start.offset + size,
            chain,
        };
        Ok(Plan {
            wrap,
            place: start.offset % area,
            prefix,
            next: self.head,
        })
    }
}

fn prefix(chain: u32, kind: u8, body: &[&[u8]]) -> ([u8; PREFIX_LEN], u32) {
    let kind = [kind];
    let payload = std::iter::once(&kind[..]).chain(body.iter().copied());
    let ([h0, h1, h2, h3, h4, h5, h6, h7], chain) = record::header(chain, payload);
    ([h0, h1, h2, h3, h4, h5, h6, h7, kind[0]], chain)
}

/// One step of a [`Cursor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step<'a> {
    /// The body of the next data record.
    Data(&'a [u8]),
    /// A wrap or restart record. The cursor moved; ask for the next window.
    Moved,
    /// The chain ends here. The cursor is at the head of the ring.
    End,
}

/// Walks the records of a ring from its tail at recovery. Loop: read the bytes of
/// [`window`](Self::window) from the area, give them to [`next`](Self::next), and
/// stop at [`Step::End`]. It ends on any bytes within one lap of the area.
#[derive(Debug)]
pub(crate) struct Cursor {
    layout: Layout,
    tail: u64,
    at: Position,
}

impl Cursor {
    /// Starts at `tail`, the boundary before the oldest live record.
    pub(crate) fn new(layout: Layout, tail: Position) -> Self {
        Self {
            layout,
            tail: tail.offset,
            at: tail,
        }
    }

    /// The boundary after the last record read. After [`Step::End`] it is the head
    /// for [`Writer::new`].
    pub(crate) fn position(&self) -> Position {
        self.at
    }

    /// The place in the area and the count of bytes to read for the next step.
    pub(crate) fn window(&self) -> (u64, u64) {
        let area = self.layout.area;
        let place = self.at.offset % area;
        let unread = area - (self.at.offset - self.tail);
        let len = self.layout.window_max().min(area - place).min(unread);
        (place, len)
    }

    /// Reads the record at the start of `bytes`, the bytes of the last
    /// [`window`](Self::window).
    ///
    /// # Panics
    ///
    /// When `bytes` is not the window.
    pub(crate) fn next<'a>(&mut self, bytes: &'a [u8]) -> Step<'a> {
        let (place, len) = self.window();
        assert!(
            u64::try_from(bytes.len()) == Ok(len),
            "invariant: got {} bytes for a window of {len} at {place}",
            bytes.len()
        );
        let Some(record) = record::read(bytes, self.at.chain) else {
            return Step::End;
        };
        let size =
            u64::try_from(record.size).expect("invariant: a record size fits in u64");
        let area = self.layout.area;
        let rest = area - place;
        match *record.payload {
            [DATA, ref body @ ..] => {
                self.at = Position {
                    offset: self.at.offset + size,
                    chain: record.crc,
                };
                Step::Data(body)
            }
            [WRAP] if self.at.offset - self.tail + rest <= area => {
                self.at = Position {
                    offset: self.at.offset + rest,
                    chain: record.crc,
                };
                Step::Moved
            }
            [RESTART, c0, c1, c2, c3] => {
                self.at = Position {
                    offset: self.at.offset + size,
                    chain: u32::from_le_bytes([c0, c1, c2, c3]),
                };
                Step::Moved
            }
            _ => Step::End,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    const BLOCKS: u64 = 8;
    const AREA: u64 = BLOCKS * ALIGN_U64;
    const BODY_MAX: usize = 3 * ALIGN;

    const START: Position = Position {
        offset: 0,
        chain: 0x5EED_0001,
    };

    fn layout() -> Layout {
        Layout::new(AREA, BODY_MAX)
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
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
        fn new() -> Self {
            Self {
                area: vec![0; index(AREA)],
                writer: Writer::new(layout(), START.offset, START),
                tail: START,
                live: VecDeque::new(),
            }
        }

        /// Writes `bytes` at `place`. `used` bytes from the tail are live.
        fn write(&mut self, place: u64, bytes: &[u8], used: u64) {
            let len = u64::try_from(bytes.len()).expect("the length fits in u64");
            assert_eq!(place % ALIGN_U64, 0, "a write starts off a block boundary");
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
            if let Some((place, prefix)) = plan.wrap {
                self.write(place, &prefix, used);
            }
            let mut bytes = plan.prefix.to_vec();
            bytes.extend_from_slice(body);
            self.write(plan.place, &bytes, used);
            let data = data.then(|| body.to_vec());
            self.live.push_back(Live { start, data });
        }

        fn append(&mut self, body: &[u8]) -> Result<Plan, Full> {
            let start = self.writer.head();
            let plan = self.writer.append(&[body])?;
            self.apply(start, &plan, body, true);
            Ok(plan)
        }

        fn restart(&mut self, chain: u32) -> Result<Plan, Full> {
            let start = self.writer.head();
            let plan = self.writer.restart(chain)?;
            self.apply(start, &plan, &chain.to_le_bytes(), false);
            Ok(plan)
        }

        fn release(&mut self, count: usize) {
            self.live.drain(..count.min(self.live.len()));
            let head = self.writer.head();
            self.tail = self.live.front().map_or(head, |live| live.start);
            self.writer.release(self.tail.offset);
        }

        fn data(&self) -> Vec<Vec<u8>> {
            let data = self.live.iter();
            data.filter_map(|live| live.data.clone()).collect()
        }

        /// Drops the writer, as a crash does, and continues from what recovery
        /// finds.
        fn reopen(&mut self) -> Vec<Vec<u8>> {
            let (data, head) = recover(&self.area, self.tail);
            self.writer = Writer::new(layout(), self.tail.offset, head);
            data
        }
    }

    /// Walks the area with the real cursor.
    fn recover(area: &[u8], tail: Position) -> (Vec<Vec<u8>>, Position) {
        let mut cursor = Cursor::new(layout(), tail);
        let mut data = Vec::new();
        for _ in 0..=2 * BLOCKS {
            let (place, len) = cursor.window();
            match cursor.next(&area[index(place)..index(place + len)]) {
                Step::Data(body) => data.push(body.to_vec()),
                Step::Moved => {}
                Step::End => return (data, cursor.position()),
            }
        }
        panic!("the cursor did not end within two steps per block");
    }

    #[derive(Clone, Debug)]
    enum Op {
        Append(Vec<u8>),
        Restart(u32),
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
            1 => any::<u32>().prop_map(Op::Restart),
            2 => (0..4usize).prop_map(Op::Release),
        ];
        prop::collection::vec(op, 0..40)
    }

    /// Runs `ops` and checks each refusal against the model.
    fn run(ring: &mut Ring, ops: &[Op]) {
        for op in ops {
            let head = ring.writer.head();
            let result = match op {
                Op::Append(body) => ring.append(body).map(drop),
                Op::Restart(chain) => ring.restart(*chain).map(drop),
                Op::Release(count) => {
                    ring.release(*count);
                    Ok(())
                }
            };
            if let Err(full) = result {
                let free = AREA - (head.offset - ring.tail.offset);
                assert_eq!(full.free, free, "free bytes at {op:?}");
                assert!(full.needed > free, "a record that fits was refused");
                assert_eq!(ring.writer.head(), head, "a refusal moved the head");
            }
        }
    }

    mod layout {
        use super::*;

        #[test]
        #[should_panic(expected = "a ring of 4097 bytes does not hold whole blocks")]
        fn panics_on_an_area_of_part_blocks() {
            let _layout = Layout::new(4097, 1);
        }

        #[test]
        #[should_panic(expected = "and a record of 8184 bytes")]
        fn panics_on_a_record_larger_than_the_area() {
            let _layout = Layout::new(8192, 8184);
        }
    }

    mod writer {
        use super::*;

        fn at(blocks: u64) -> Position {
            Position {
                offset: blocks * 4096,
                chain: 9,
            }
        }

        #[test]
        fn places_records_back_to_back_from_the_head() {
            let mut writer = Writer::new(layout(), 0, START);
            let first = writer.append(&[b"a"]).expect("the ring is empty");
            let second = writer.append(&[&[7; ALIGN]]).expect("the ring has room");
            assert_eq!(
                (first.wrap, first.place, first.next.offset),
                (None, 0, 4096)
            );
            assert_eq!((second.wrap, second.place), (None, 4096));
            assert_eq!(second.next.offset, 3 * 4096);
            assert_eq!(writer.head(), second.next);
        }

        #[test]
        fn fills_a_block_with_a_body_of_4087_bytes() {
            let mut writer = Writer::new(layout(), 0, START);
            let fits = writer.append(&[&[7; 4087]]).expect("the ring is empty");
            let spills = writer.append(&[&[7; 4088]]).expect("the ring has room");
            assert_eq!((fits.next.offset, spills.next.offset), (4096, 3 * 4096));
        }

        #[test]
        fn puts_a_wrap_record_when_a_record_does_not_fit_in_the_rest() {
            let mut writer = Writer::new(layout(), 6 * 4096, at(7));
            let plan = writer.append(&[&[7; ALIGN]]).expect("the ring has room");
            assert_eq!(plan.wrap.map(|(place, _)| place), Some(7 * 4096));
            assert_eq!((plan.place, plan.next.offset), (0, 10 * 4096));
        }

        #[test]
        fn refuses_a_record_that_does_not_fit_before_the_tail() {
            let mut writer = Writer::new(layout(), 4096, at(7));
            let full = Full {
                needed: 3 * 4096,
                free: 2 * 4096,
            };
            assert_eq!(writer.append(&[&[7; ALIGN]]), Err(full));
            assert_eq!(writer.head(), at(7));
            writer.release(2 * 4096);
            assert_eq!(writer.append(&[&[7; ALIGN]]).map(|plan| plan.place), Ok(0));
        }

        #[test]
        fn continues_the_chain_from_the_restart_value() {
            let mut writer = Writer::new(layout(), 0, START);
            let plan = writer.restart(77).expect("the ring is empty");
            let next = Position {
                offset: 4096,
                chain: 77,
            };
            assert_eq!((plan.next, writer.head()), (next, next));
        }

        #[test]
        #[should_panic(
            expected = "a record of 12289 bytes is over the maximum of 12288"
        )]
        fn panics_on_a_body_over_the_maximum() {
            let mut writer = Writer::new(layout(), 0, START);
            let _plan = writer.append(&[&[0; BODY_MAX + 1]]);
        }

        #[test]
        #[should_panic(expected = "release to 8192 is outside the live records from 0")]
        fn panics_on_a_release_past_the_head() {
            let mut writer = Writer::new(layout(), 0, START);
            writer.append(&[b"a"]).expect("the ring is empty");
            writer.release(8192);
        }

        #[test]
        #[should_panic(expected = "live records from 0 to 36864 do not fit a ring")]
        fn panics_on_live_records_larger_than_the_area() {
            let _writer = Writer::new(layout(), 0, at(9));
        }
    }

    mod cursor {
        use super::*;

        #[test]
        fn finds_no_record_in_a_new_area() {
            let ring = Ring::new();
            assert_eq!(recover(&ring.area, START), (vec![], START));
        }

        /// The log is cut at a damaged record and restarted. A new record with the
        /// bytes of the old one goes to the same place, before an old record.
        #[test]
        fn drops_an_old_record_after_a_cut_and_a_restart() {
            let mut ring = Ring::new();
            for body in [&b"one"[..], b"two", b"three", b"four"] {
                ring.append(body).expect("the ring has room");
            }
            ring.area[4096 + PREFIX_LEN] ^= 1;
            assert_eq!(ring.reopen(), [b"one"]);
            ring.live.truncate(1);
            ring.restart(0xABCD).expect("the ring has room");
            let plan = ring.append(b"three").expect("the ring has room");
            assert_eq!(plan.place, 2 * 4096);
            assert_eq!(ring.reopen(), [&b"one"[..], b"three"]);
        }

        #[test]
        fn asks_for_no_bytes_past_one_lap() {
            let mut ring = Ring::new();
            for _ in 0..BLOCKS - 1 {
                ring.append(b"a").expect("the ring has room");
            }
            let mut cursor = Cursor::new(layout(), START);
            for _ in 0..BLOCKS - 1 {
                let (place, len) = cursor.window();
                cursor.next(&ring.area[index(place)..index(place + len)]);
            }
            assert_eq!(cursor.window(), (7 * 4096, 4096));
            ring.append(b"a").expect("the ring has room");
            cursor.next(&ring.area[7 * ALIGN..]);
            assert_eq!(cursor.window(), (0, 0));
            assert_eq!(cursor.next(&[]), Step::End);
        }

        #[test]
        fn ends_at_a_wrap_record_that_passes_the_tail() {
            let mut area = vec![0; index(AREA)];
            let tail = Position {
                offset: 4 * 4096,
                chain: 5,
            };
            let (first, chain) = prefix(tail.chain, WRAP, &[]);
            let (second, _) = prefix(chain, WRAP, &[]);
            area[4 * ALIGN..4 * ALIGN + PREFIX_LEN].copy_from_slice(&first);
            area[..PREFIX_LEN].copy_from_slice(&second);
            let lap = Position {
                offset: AREA,
                chain,
            };
            assert_eq!(recover(&area, tail), (vec![], lap));
        }

        #[test]
        fn ends_at_a_record_of_a_kind_it_does_not_know() {
            let mut area = vec![0; index(AREA)];
            let (unknown, _) = prefix(START.chain, 4, &[]);
            area[..PREFIX_LEN].copy_from_slice(&unknown);
            assert_eq!(recover(&area, START), (vec![], START));
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
                let (data, head) = recover(&ring.area, ring.tail);
                prop_assert_eq!(data, ring.data());
                prop_assert_eq!(head, ring.writer.head());
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
                    let written = PREFIX_LEN + last.len();
                    ring.area[index(plan.place) + flip.index(written)] ^= 1;
                }
                prop_assert_eq!(ring.reopen(), expected.clone());
                prop_assume!(ring.restart(1).is_ok() && ring.append(&after).is_ok());
                expected.push(after);
                prop_assert_eq!(ring.reopen(), expected);
            }

            #[test]
            fn ends_on_any_bytes(
                heads in any::<[(u16, u32, u8, [u8; 4]); 8]>(),
                tail in 0..BLOCKS,
                chain in any::<u32>(),
            ) {
                let mut area = vec![0; index(AREA)];
                for (block, (len, crc, kind, body)) in heads.into_iter().enumerate() {
                    let mut bytes = u32::from(len % 16384).to_le_bytes().to_vec();
                    bytes.extend(crc.to_le_bytes());
                    bytes.push(kind % 5);
                    bytes.extend(body);
                    let at = block * ALIGN;
                    area[at..at + bytes.len()].copy_from_slice(&bytes);
                }
                let tail = Position { offset: tail * ALIGN_U64, chain };
                let (_, head) = recover(&area, tail);
                prop_assert!(head.offset - tail.offset <= AREA);
            }
        }
    }
}
