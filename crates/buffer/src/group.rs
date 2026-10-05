//! The entries of one group commit and the record that holds them.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::iter;
use std::slice;

use block::{Block, Pool, Unique};
use types::channel::Slot;

use crate::entry::{self, ENTRIES_MAX, Entry, Header};
use crate::record;
use crate::wal::{Full, Plan, Writer};

/// Bytes of the block that holds a record header and the largest entry table: one
/// block of the pool's 64 KiB class.
pub(crate) const META_LEN: usize = record::HEADER_LEN + entry::TABLE_MAX;
const _: () = assert!(META_LEN <= 1 << 16, "the table fits one 64 KiB block");

/// The entries of one group commit, in append order. The first push takes the block
/// for the record header and the table and the block for a wrap header, so that
/// [`close`](Self::close) and [`Closed::seal`] allocate nothing. [`Sealed::clear`]
/// gives the group back with its capacity, so groups that alternate between the
/// handle and the commit task make no heap allocation per commit.
#[derive(Debug, Default)]
pub(crate) struct Group {
    headers: Vec<Header>,
    slots: Vec<Slot>,
    /// The bytes of every entry, in order. At close the record header and the
    /// table go in front.
    writes: Vec<Block>,
    /// The bytes of every entry together.
    bytes: usize,
    meta: Option<Unique>,
    wrap: Option<Unique>,
}

/// Why a group did not take a batch. Nothing changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The record with the batch would be over the layout's maximum body, over
    /// [`ENTRIES_MAX`] entries or parts, or past the room in the ring. The caller
    /// closes the group and pushes the batch into the next one.
    Record,
    /// The ring has no room for the batch's own record. The group is empty.
    Ring(Full),
    /// The pool has no block for the record header of a new group.
    Pool(block::Error),
}

impl Group {
    pub(crate) fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// The record body with `count` more entries of `bytes` bytes together: the
    /// table and every entry's bytes.
    fn body_len_with(&self, count: usize, bytes: usize) -> usize {
        self.headers
            .len()
            .checked_add(count)
            .map(entry::table_len)
            .and_then(|table| table.checked_add(self.bytes))
            .and_then(|body| body.checked_add(bytes))
            .expect("invariant: a body fits in usize")
    }

    /// Adds every entry of `batch`, or none, when the record with them fits the
    /// layout and the ring of `writer`; it sets each header's `bytes`. The first
    /// entry takes the group's blocks from `pool`.
    ///
    /// # Errors
    ///
    /// [`Rejected`] when the batch does not fit or the pool has no block. Nothing
    /// changes.
    ///
    /// # Panics
    ///
    /// When the batch alone is over the layout's maximum body or [`ENTRIES_MAX`].
    pub(crate) fn push<'a>(
        &mut self,
        pool: &Pool,
        writer: &Writer,
        batch: impl IntoIterator<Item = &'a Entry<'a>, IntoIter: Clone>,
    ) -> Result<(), Rejected> {
        let batch = batch.into_iter();
        let count = batch.clone().count();
        if count == 0 {
            return Ok(());
        }
        let parts = batch.clone().map(|entry| entry.parts.len()).sum::<usize>();
        let len = batch
            .clone()
            .flat_map(|entry| entry.parts)
            .map(|part| part.len())
            .sum::<usize>();
        let body = self.body_len_with(count, len);
        if body > writer.body_max() {
            assert!(
                !self.is_empty(),
                "invariant: a batch of {count} entries and {len} bytes is over the \
                 maximum body of {} bytes",
                writer.body_max()
            );
            return Err(Rejected::Record);
        }
        let over = |have: usize, more: usize| have.saturating_add(more) > ENTRIES_MAX;
        if over(self.headers.len(), count) || over(self.writes.len(), parts) {
            assert!(
                !self.is_empty(),
                "invariant: a batch of {count} entries and {parts} parts is over \
                 the most of {ENTRIES_MAX}"
            );
            return Err(Rejected::Record);
        }
        if let Err(full) = writer.fits(body) {
            return Err(if self.is_empty() {
                Rejected::Ring(full)
            } else {
                Rejected::Record
            });
        }
        if self.meta.is_none() {
            let meta = pool.alloc(META_LEN).map_err(Rejected::Pool)?;
            let wrap = pool.alloc(record::HEADER_LEN).map_err(Rejected::Pool)?;
            self.meta = Some(meta);
            self.wrap = Some(wrap);
        }
        for entry in batch {
            self.headers.push(entry.header());
            self.slots.push(entry.slot);
            self.writes.extend(entry.parts.iter().cloned());
        }
        self.bytes = self
            .bytes
            .checked_add(len)
            .expect("invariant: a body fits in usize");
        Ok(())
    }

    /// Places the record with `writer` and writes the entry table at the end of the
    /// group's block, in front of the parts. [`Closed::seal`] makes the headers.
    ///
    /// # Panics
    ///
    /// When the group is empty.
    pub(crate) fn close(mut self, writer: &mut Writer) -> Closed {
        let (Some(mut meta), Some(wrap)) = (self.meta.take(), self.wrap.take()) else {
            panic!("invariant: an empty group has no record");
        };
        let table = entry::table_len(self.headers.len());
        let used = record::HEADER_LEN
            .checked_add(table)
            .expect("invariant: a table fits in memory");
        let start = meta.len().checked_sub(used).unwrap_or_else(|| {
            panic!(
                "invariant: a block of {} bytes holds no header and table of {used} \
                 bytes",
                meta.len()
            )
        });
        let (_, tail) = meta.split_at_mut(start);
        let (_, table) = tail.split_at_mut(record::HEADER_LEN);
        entry::write_table(&self.headers, table);
        let body = table
            .len()
            .checked_add(self.bytes)
            .expect("invariant: a body fits in usize");
        let plan = writer.append(body).unwrap_or_else(|full| {
            panic!(
                "invariant: a closed group needs {} bytes of the ring, {} are free",
                full.needed, full.free
            )
        });
        Closed {
            group: self,
            plan,
            meta,
            wrap,
            start,
        }
    }
}

/// A group whose record has its place in the ring and its table in the meta
/// block. [`seal`](Self::seal) makes the headers once the chain value before the
/// record is known.
#[derive(Debug)]
pub(crate) struct Closed {
    group: Group,
    plan: Plan,
    meta: Unique,
    /// The block for the wrap header. It goes back to the pool when the record
    /// does not wrap.
    wrap: Unique,
    /// Where the record header starts in `meta`.
    start: usize,
}

impl Closed {
    /// Makes the record's headers from `chain`, the chain value of the record before
    /// it, and returns the chain value after the record.
    pub(crate) fn seal(self, chain: u32) -> (Sealed, u32) {
        let Self {
            mut group,
            plan,
            mut meta,
            mut wrap,
            start,
        } = self;
        let (_, tail) = meta.split_at_mut(start);
        let (header, table) = tail.split_at_mut(record::HEADER_LEN);
        let parts = group.writes.iter().map(|part| &**part);
        let body = iter::once(&*table).chain(parts);
        let (sealed, next) = plan.seal(chain, body);
        header.copy_from_slice(&sealed.record.header);
        group.writes.insert(0, meta.freeze().skip(start));
        let wrap = sealed.wrap.map(|write| {
            let start =
                wrap.len()
                    .checked_sub(record::HEADER_LEN)
                    .unwrap_or_else(|| {
                        panic!(
                            "invariant: a block of {} bytes holds no wrap header",
                            wrap.len()
                        )
                    });
            let (_, header) = wrap.split_at_mut(start);
            header.copy_from_slice(&write.header);
            (write.place, wrap.freeze().skip(start))
        });
        let sealed = Sealed {
            group,
            place: plan.place,
            next: plan.next,
            wrap,
        };
        (sealed, next)
    }
}

/// A group whose record is complete: each of its [`writes`](Self::writes) goes at
/// its place in the area.
#[derive(Debug)]
pub(crate) struct Sealed {
    group: Group,
    place: u64,
    next: u64,
    wrap: Option<(u64, Block)>,
}

impl Sealed {
    /// The writes that put the record in the ring, in order: the wrap record when
    /// the record wraps, then the record's header, table, and entries back to back.
    pub(crate) fn writes(&self) -> impl Iterator<Item = (u64, &[Block])> {
        let wrap = self
            .wrap
            .as_ref()
            .map(|(place, block)| (*place, slice::from_ref(block)));
        wrap.into_iter()
            .chain(iter::once((self.place, &*self.group.writes)))
    }

    /// The offset after the record.
    #[cfg_attr(not(test), expect(dead_code, reason = "trimming moves the tail"))]
    pub(crate) fn next(&self) -> u64 {
        self.next
    }

    /// The headers, for the durable tails once the record is synced.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.group.headers
    }

    /// The slot of each header, in the same order.
    pub(crate) fn slots(&self) -> &[Slot] {
        &self.group.slots
    }

    /// Drops the entries and the blocks and gives the group back with its capacity.
    pub(crate) fn clear(self) -> Group {
        let mut group = self.group;
        group.headers.clear();
        group.slots.clear();
        group.writes.clear();
        group.bytes = 0;
        group
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use block::{Config, Heap};
    use proptest::prelude::*;
    use types::channel;
    use types::frame::Path;
    use types::hash;
    use types::time::Stamp;

    use crate::entry::table_len;
    use crate::record::HEADER_LEN;
    use crate::wal::{self, Cursor, Layout, Position, Step, Window};

    const BLOCKS: u64 = 32;
    const AREA: u64 = BLOCKS * 4096;
    const BODY_MAX: usize = 8192;
    const CHAIN: u32 = 0x5EED_0001;

    fn pool(budget: usize) -> Pool {
        let config = Config { budget };
        Pool::new(config.clone(), Heap::new(config.reservation()))
    }

    fn block(pool: &Pool, bytes: &[u8]) -> Block {
        let mut unique = pool.alloc(bytes.len()).expect("the pool has room");
        unique.copy_from_slice(bytes);
        unique.freeze()
    }

    fn key(value: u128) -> channel::Key {
        channel::Key::from_u128(value)
    }

    fn slot(value: u128) -> Slot {
        Slot::new(u32::try_from(value).expect("a small index"))
    }

    fn header(index: u128, path: Path, first: u64) -> Header {
        Header {
            index: key(index),
            path,
            first,
            len: 2,
            stored_at: Stamp::from_nanos(5),
            last: Some(Stamp::from_nanos(9)),
            tag: 3,
            bytes: 99,
        }
    }

    /// The entry that `push` makes `header` from; `bytes` comes from the parts.
    fn entry(header: Header, parts: &[Block]) -> Entry<'_> {
        Entry {
            index: header.index,
            slot: slot(header.index.as_u128()),
            path: header.path,
            first: header.first,
            len: header.len,
            stored_at: header.stored_at,
            last: header.last,
            tag: header.tag,
            parts,
        }
    }

    fn start() -> Position {
        Position::new(0, CHAIN).expect("offset 0 is aligned")
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    /// The smallest body a header can open holds one entry of no bytes.
    #[test]
    fn a_ring_that_a_header_opens_holds_one_entry() {
        use crate::header;
        let small = Layout::new(AREA, table_len(1)).expect("the sizes make a ring");
        let block = header::Header::new(small, CHAIN).encode();
        let opened = header::Header::decode(&block, &[0; 4096]).expect("a whole block");
        let bytes = vec![0; index(AREA)];
        let mut cursor = Cursor::new(opened.layout, opened.tail, 1 << 16);
        let Window { place, len } = cursor.window();
        let step = cursor.next(&bytes[index(place)..index(place) + len]);
        assert_eq!(step, Ok(Step::End));
        let (writer, _) = cursor
            .writer(opened.tail.offset(), 1)
            .expect("the ring is empty");
        let entry = entry(header(1, Path::Live, 0), &[]);
        let pushed = Group::default().push(&pool(1 << 20), &writer, &[entry]);
        assert_eq!(pushed, Ok(()));
    }

    /// An area in memory and the writer that continues it, as a ring just made and
    /// opened: it holds one restart record. `chain` is the chain value at the head.
    struct Area {
        bytes: Vec<u8>,
        writer: Writer,
        chain: u32,
        pool: Pool,
        layout: Layout,
    }

    impl Area {
        fn new() -> Self {
            Self::with_body_max(BODY_MAX)
        }

        fn with_body_max(body_max: usize) -> Self {
            let bytes = vec![0; index(AREA)];
            let layout = Layout::new(AREA, body_max).expect("the sizes make a ring");
            let mut cursor = Cursor::new(layout, start(), 1 << 16);
            let Window { place, len } = cursor.window();
            let step = cursor.next(&bytes[index(place)..index(place) + len]);
            assert_eq!(step, Ok(Step::End), "a zeroed area ends at once");
            let (writer, sealed) = cursor.writer(0, 1).expect("the ring is empty");
            let body = 1u32.to_le_bytes();
            let mut area = Self {
                bytes,
                writer,
                chain: 1,
                pool: pool(1 << 22),
                layout,
            };
            area.write(&sealed, [&body[..]]);
            area
        }

        /// Writes a sealed record, with `body` as the record's body.
        fn write<'a>(
            &mut self,
            sealed: &wal::Sealed,
            body: impl IntoIterator<Item = &'a [u8]>,
        ) {
            if let Some(wrap) = sealed.wrap {
                let at = index(wrap.place);
                self.bytes[at..at + HEADER_LEN].copy_from_slice(&wrap.header);
            }
            let mut at = index(sealed.record.place);
            self.bytes[at..at + HEADER_LEN].copy_from_slice(&sealed.record.header);
            at += HEADER_LEN;
            for part in body {
                self.bytes[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
        }

        /// Seals and writes a closed group's record as the commit task does: each
        /// write's blocks back to back at its place.
        fn commit(&mut self, closed: Closed) -> Sealed {
            let (sealed, chain) = closed.seal(self.chain);
            self.chain = chain;
            self.put(&sealed);
            sealed
        }

        fn put(&mut self, sealed: &Sealed) {
            for (place, blocks) in sealed.writes() {
                let mut at = index(place);
                for part in blocks {
                    self.bytes[at..at + part.len()].copy_from_slice(part);
                    at += part.len();
                }
            }
        }

        fn push(&mut self, group: &mut Group, header: Header, parts: &[Block]) {
            group
                .push(&self.pool, &self.writer, &[entry(header, parts)])
                .expect("the ring has room");
        }

        /// The data bodies, as recovery reads them.
        fn walk(&self) -> Vec<Vec<u8>> {
            self.walk_from(start())
        }

        fn walk_from(&self, tail: Position) -> Vec<Vec<u8>> {
            let mut cursor = Cursor::new(self.layout, tail, 1 << 16);
            let mut bodies = Vec::new();
            loop {
                let Window { place, len } = cursor.window();
                let window = &self.bytes[index(place)..index(place) + len];
                match cursor
                    .next(window)
                    .expect("the ring holds what was written")
                {
                    Step::Data(body) => {
                        let start = index(place) + HEADER_LEN;
                        bodies.push(self.bytes[start..start + body.len].to_vec());
                    }
                    Step::Moved | Step::More => {}
                    Step::End => return bodies,
                }
            }
        }
    }

    /// A pushed entry and its bytes.
    type Stored = (Header, Vec<u8>);

    /// One entry of a model log: which path it is on, how far past the tail it
    /// starts, and what it carries.
    #[derive(Clone, Debug)]
    struct Next {
        index: u128,
        path: Path,
        skip: u64,
        len: u32,
        last: Option<i64>,
        tag: u8,
        parts: Vec<Vec<u8>>,
    }

    fn next() -> impl Strategy<Value = Next> {
        (
            0..3u128,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..3u64,
            0..5u32,
            prop::option::of(any::<i64>()),
            any::<u8>(),
            prop::collection::vec(prop::collection::vec(any::<u8>(), 0..100), 0..3),
        )
            .prop_map(|(index, path, skip, len, last, tag, parts)| Next {
                index,
                path,
                skip,
                len,
                last,
                tag,
                parts,
            })
    }

    proptest! {
        #[test]
        fn recovery_reads_back_what_was_pushed(
            groups in prop::collection::vec(prop::collection::vec(next(), 1..8), 1..5),
        ) {
            let mut area = Area::new();
            let mut seqs: hash::Map<(u128, Path), u64> = hash::Map::default();
            let mut group = Group::default();
            let mut expected: Vec<Vec<Stored>> = Vec::new();
            for entries in groups {
                let mut stored = Vec::new();
                let mut slots = Vec::new();
                for next in entries {
                    let seq = seqs.entry((next.index, next.path)).or_default();
                    let bytes = next.parts.concat();
                    let header = Header {
                        index: key(next.index),
                        path: next.path,
                        first: *seq + next.skip,
                        len: next.len,
                        stored_at: Stamp::from_nanos(5),
                        last: next.last.map(Stamp::from_nanos),
                        tag: next.tag,
                        bytes: u32::try_from(bytes.len()).expect("under 300 bytes"),
                    };
                    let blocks: Vec<Block> =
                        next.parts.iter().map(|part| block(&area.pool, part)).collect();
                    area.push(&mut group, header, &blocks);
                    *seq = header.first + u64::from(header.len);
                    stored.push((header, bytes));
                    slots.push(slot(next.index));
                }
                let closed = group.close(&mut area.writer);
                let sealed = area.commit(closed);
                let headers: Vec<Header> = stored.iter().map(|(header, _)| *header).collect();
                prop_assert_eq!(sealed.headers(), headers.as_slice());
                prop_assert_eq!(sealed.slots(), slots.as_slice());
                group = sealed.clear();
                prop_assert!(group.is_empty());
                expected.push(stored);
            }

            let bodies = area.walk();
            prop_assert_eq!(bodies.len(), expected.len());
            for (body, stored) in bodies.iter().zip(&expected) {
                let bytes: usize = stored.iter().map(|(_, bytes)| bytes.len()).sum();
                prop_assert_eq!(body.len(), table_len(stored.len()) + bytes);
                let entries = entry::parsed(body).expect("every entry is whole");
                let stored: Vec<(Header, &[u8])> =
                    stored.iter().map(|(header, bytes)| (*header, bytes.as_slice())).collect();
                prop_assert_eq!(entries, stored);
            }
        }
    }

    #[test]
    fn a_closed_group_holds_the_header_the_table_and_the_parts() {
        let mut area = Area::new();
        let mut group = Group::default();
        let parts = [block(&area.pool, b"abc"), block(&area.pool, b"de")];
        let first = Header {
            bytes: 3,
            ..header(1, Path::Live, 0)
        };
        let second = Header {
            bytes: 2,
            ..header(2, Path::Backfill, 7)
        };
        area.push(&mut group, first, &parts[..1]);
        area.push(&mut group, second, &parts[1..]);
        let (sealed, _) = group.close(&mut area.writer).seal(area.chain);
        let writes: Vec<(u64, Vec<&[u8]>)> = sealed
            .writes()
            .map(|(place, blocks)| (place, blocks.iter().map(|part| &**part).collect()))
            .collect();
        let [(place, parts)] = writes.as_slice() else {
            panic!("one write, got {writes:?}");
        };
        assert_eq!(*place, 4096, "after the restart record");
        let mut meta = parts[0][..HEADER_LEN].to_vec();
        meta.resize(HEADER_LEN + table_len(2), 0);
        entry::write_table(&[first, second], &mut meta[HEADER_LEN..]);
        assert_eq!(parts, &[meta.as_slice(), b"abc", b"de"]);
        assert_eq!(sealed.headers(), [first, second]);
        assert_eq!(sealed.slots(), [slot(1), slot(2)]);
    }

    /// Two groups closed before either is sealed read back when the second is
    /// sealed from the chain value after the first, and not when it is sealed from
    /// the value before the first.
    #[test]
    fn seal_chains_each_record_from_the_one_before() {
        let close_two = |area: &mut Area| {
            let mut first = Group::default();
            let mut second = Group::default();
            let a = block(&area.pool, b"a");
            let b = block(&area.pool, b"b");
            area.push(&mut first, header(1, Path::Live, 0), &[a]);
            area.push(&mut second, header(2, Path::Live, 0), &[b]);
            (
                first.close(&mut area.writer),
                second.close(&mut area.writer),
            )
        };
        let mut right = Area::new();
        let (first, second) = close_two(&mut right);
        let (a, chain) = first.seal(right.chain);
        let (b, _) = second.seal(chain);
        right.put(&a);
        right.put(&b);
        assert_eq!(right.walk().len(), 2);
        let mut wrong = Area::new();
        let (first, second) = close_two(&mut wrong);
        let (a, _) = first.seal(wrong.chain);
        let (b, _) = second.seal(wrong.chain);
        wrong.put(&a);
        wrong.put(&b);
        assert_eq!(
            wrong.walk().len(),
            1,
            "the second is sealed from the wrong chain"
        );
    }

    /// Ten records of three blocks follow the restart block, so a record of two
    /// blocks does not fit in the last block and wraps once the tail moves past
    /// the first record.
    #[test]
    fn a_closed_group_holds_the_wrap_header_when_the_record_wraps() {
        let mut area = Area::new();
        let mut group = Group::default();
        let mut tail = start();
        for first in 0..10 {
            let part = block(&area.pool, &vec![7; BODY_MAX - table_len(1)]);
            area.push(&mut group, header(1, Path::Live, 2 * first), &[part]);
            let closed = group.close(&mut area.writer);
            let sealed = area.commit(closed);
            if first == 0 {
                tail = Position::new(sealed.next(), area.chain).expect("aligned");
            }
            group = sealed.clear();
        }
        assert_eq!(area.writer.head(), 31 * 4096);
        assert_eq!(tail.offset(), 4 * 4096);
        area.writer.release(tail.offset());
        let part = block(&area.pool, &[7; 4096]);
        area.push(&mut group, header(1, Path::Live, 100), &[part]);
        let closed = group.close(&mut area.writer);
        let sealed = area.commit(closed);
        let writes: Vec<(u64, usize)> = sealed
            .writes()
            .map(|(place, blocks)| (place, blocks.len()))
            .collect();
        assert_eq!(
            writes,
            [(31 * 4096, 1), (0, 2)],
            "the wrap, then the record"
        );
        let bodies = area.walk_from(tail);
        assert_eq!(bodies.len(), 10);
        assert_eq!(bodies[9].len(), table_len(1) + 4096);
    }

    #[test]
    fn push_sets_the_bytes_of_the_header_from_its_parts() {
        let mut area = Area::new();
        let mut group = Group::default();
        let parts = [block(&area.pool, b"abcd"), block(&area.pool, b"")];
        area.push(&mut group, header(1, Path::Live, 0), &parts);
        area.push(&mut group, header(1, Path::Live, 2), &[]);
        let bytes: Vec<u32> = group.headers.iter().map(|header| header.bytes).collect();
        assert_eq!(bytes, [4, 0]);
        assert_eq!(group.body_len_with(1, 10), table_len(3) + 14);
    }

    #[test]
    fn the_meta_block_holds_the_record_header_and_the_largest_table() {
        assert_eq!(META_LEN, HEADER_LEN + table_len(ENTRIES_MAX));
    }

    #[test]
    fn a_group_with_an_entry_of_no_bytes_is_not_empty() {
        let mut area = Area::new();
        let mut group = Group::default();
        area.push(&mut group, header(1, Path::Live, 0), &[]);
        assert!(!group.is_empty());
    }

    #[test]
    fn an_entry_over_the_record_asks_for_a_close() {
        let mut area = Area::new();
        let mut group = Group::default();
        let half = (BODY_MAX - table_len(2)) / 2 + 1;
        let big = block(&area.pool, &vec![7; half]);
        area.push(
            &mut group,
            header(1, Path::Live, 0),
            std::slice::from_ref(&big),
        );
        let rejected = group.push(
            &area.pool,
            &area.writer,
            &[entry(header(1, Path::Live, 2), &[big])],
        );
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        assert_eq!(group.bytes, half);
    }

    #[test]
    fn an_entry_past_the_most_entries_asks_for_a_close() {
        let area = Area::with_body_max(60_000);
        let mut group = Group::default();
        let mut push = |first| {
            group.push(
                &area.pool,
                &area.writer,
                &[entry(header(1, Path::Live, first), &[])],
            )
        };
        for first in 0..ENTRIES_MAX {
            assert_eq!(push(u64::try_from(first).expect("small")), Ok(()));
        }
        assert_eq!(push(9999), Err(Rejected::Record));
        assert_eq!(group.headers.len(), ENTRIES_MAX);
        const { assert!(META_LEN <= 1 << 16, "the table fits one 64 KiB block") };
    }

    #[test]
    fn an_entry_the_ring_has_no_room_for_changes_nothing() {
        let mut area = Area::new();
        let mut group = Group::default();
        let part = block(&area.pool, &vec![7; BODY_MAX - table_len(1)]);
        let one = header(1, Path::Live, 0);
        let rejected = loop {
            let pushed = group.push(
                &area.pool,
                &area.writer,
                &[entry(one, std::slice::from_ref(&part))],
            );
            match pushed {
                Ok(()) => {
                    let closed = group.close(&mut area.writer);
                    group = area.commit(closed).clear();
                }
                Err(rejected) => break rejected,
            }
        };
        // Ten records of three blocks follow the restart block and leave one block,
        // which the next record skips to wrap.
        let full = Full {
            needed: 4 * 4096,
            free: 4096,
        };
        assert_eq!(rejected, Rejected::Ring(full));
        assert!(group.is_empty());
        assert_eq!(group.bytes, 0);
        assert!(group.meta.is_none(), "a refused first entry takes no block");
    }

    #[test]
    fn a_first_entry_the_pool_has_no_block_for_changes_nothing() {
        let mut area = Area::new();
        let budget = 2 * block::footprint(META_LEN) - 1;
        area.pool = pool(budget);
        let _held = area.pool.alloc(META_LEN).expect("the first block fits");
        let available = budget - area.pool.committed();
        let mut group = Group::default();
        let rejected = group.push(
            &area.pool,
            &area.writer,
            &[entry(header(1, Path::Live, 0), &[])],
        );
        let exhausted = block::Error::Exhausted {
            requested: META_LEN,
            available,
        };
        assert_eq!(rejected, Err(Rejected::Pool(exhausted)));
        assert!(group.is_empty());
    }

    #[test]
    fn clear_keeps_the_capacity_and_gives_the_blocks_back() {
        let mut area = Area::new();
        let mut group = Group::default();
        for first in 0..3 {
            let part = block(&area.pool, b"xy");
            area.push(&mut group, header(1, Path::Live, 2 * first), &[part]);
        }
        let lent = area.pool.committed();
        let closed = group.close(&mut area.writer);
        let sealed = area.commit(closed);
        let capacity = |group: &Group| {
            (
                group.headers.capacity(),
                group.slots.capacity(),
                group.writes.capacity(),
            )
        };
        let before = capacity(&sealed.group);
        let group = sealed.clear();
        assert!(group.is_empty());
        assert_eq!(group.body_len_with(0, 0), table_len(0));
        assert_eq!(capacity(&group), before);
        area.pool.reclaim();
        assert_eq!(
            area.pool.committed(),
            lent,
            "the budget stays with the classes"
        );
        let meta = area
            .pool
            .alloc(META_LEN)
            .expect("the dropped block is free");
        assert_eq!(meta.len(), META_LEN);
    }

    #[test]
    #[should_panic(expected = "invariant: an empty group has no record")]
    fn close_of_an_empty_group_is_a_broken_invariant() {
        let mut area = Area::new();
        let _closed = Group::default().close(&mut area.writer);
    }

    #[test]
    #[should_panic(
        expected = "invariant: a batch of 1 entries and 8138 bytes is over the maximum \
                    body of 8192 bytes"
    )]
    fn a_batch_alone_over_the_record_is_a_broken_invariant() {
        let area = Area::new();
        let part = block(&area.pool, &vec![7; BODY_MAX - table_len(1) + 1]);
        let _rejected = Group::default().push(
            &area.pool,
            &area.writer,
            &[entry(header(1, Path::Live, 0), &[part])],
        );
    }

    #[test]
    #[should_panic(
        expected = "invariant: a batch of 1 entries and 1024 parts is over the most \
                    of 1023"
    )]
    fn a_batch_alone_over_the_most_parts_is_a_broken_invariant() {
        let area = Area::new();
        let parts = vec![block(&area.pool, b""); ENTRIES_MAX + 1];
        let _rejected = Group::default().push(
            &area.pool,
            &area.writer,
            &[entry(header(1, Path::Live, 0), &parts)],
        );
    }

    #[test]
    fn a_batch_goes_in_whole_or_not_at_all() {
        let mut area = Area::new();
        let mut group = Group::default();
        let third = (BODY_MAX - table_len(3)) / 3;
        let part = block(&area.pool, &vec![7; third]);
        area.push(
            &mut group,
            header(1, Path::Live, 0),
            std::slice::from_ref(&part),
        );
        let batch = [
            entry(header(1, Path::Live, 2), std::slice::from_ref(&part)),
            entry(header(2, Path::Live, 0), std::slice::from_ref(&part)),
            entry(header(3, Path::Live, 0), &[]),
        ];
        let rejected = group.push(&area.pool, &area.writer, &batch);
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        assert_eq!(group.bytes, third);
        let pushed = group.push(&area.pool, &area.writer, &batch[1..]);
        assert_eq!(pushed, Ok(()));
        assert_eq!(group.headers.len(), 3);
        assert_eq!(group.slots, [slot(1), slot(2), slot(3)]);
        assert_eq!(group.bytes, 2 * third);
    }

    /// Ten records of three blocks follow the restart block. With the restart
    /// block released, a one-block record fits the last block, but the group with a
    /// second entry would skip it and not fit: the batch is not refused, the group
    /// closes first.
    #[test]
    fn a_batch_that_fits_its_own_record_asks_for_a_close_before_full() {
        let mut area = Area::new();
        let mut group = Group::default();
        for first in 0..10 {
            let part = block(&area.pool, &vec![7; BODY_MAX - table_len(1)]);
            area.push(&mut group, header(1, Path::Live, 2 * first), &[part]);
            let closed = group.close(&mut area.writer);
            group = area.commit(closed).clear();
        }
        area.writer.release(4096);
        let big = block(&area.pool, &[7; 4000]);
        area.push(&mut group, header(1, Path::Live, 20), &[big]);
        let small = block(&area.pool, &[7; 100]);
        let batch = [entry(
            header(1, Path::Live, 22),
            std::slice::from_ref(&small),
        )];
        let rejected = group.push(&area.pool, &area.writer, &batch);
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        let closed = group.close(&mut area.writer);
        group = area.commit(closed).clear();
        assert_eq!(group.push(&area.pool, &area.writer, &batch), Ok(()));
        let (sealed, _) = group.close(&mut area.writer).seal(area.chain);
        let places: Vec<u64> = sealed.writes().map(|(place, _)| place).collect();
        assert_eq!(places, [0], "the record wraps to the first block");
    }

    #[test]
    fn an_empty_batch_changes_nothing() {
        let area = Area::new();
        let mut group = Group::default();
        let lent = area.pool.committed();
        assert_eq!(group.push(&area.pool, &area.writer, &[]), Ok(()));
        assert!(group.is_empty());
        assert!(group.meta.is_none(), "an empty batch takes no block");
        assert_eq!(area.pool.committed(), lent);
    }

    #[test]
    fn a_first_entry_the_pool_has_no_wrap_block_for_changes_nothing() {
        let mut area = Area::new();
        area.pool = pool(block::footprint(META_LEN) + 64);
        let mut group = Group::default();
        let rejected = group.push(
            &area.pool,
            &area.writer,
            &[entry(header(1, Path::Live, 0), &[])],
        );
        let exhausted = block::Error::Exhausted {
            requested: HEADER_LEN,
            available: 64,
        };
        assert_eq!(rejected, Err(Rejected::Pool(exhausted)));
        assert!(group.is_empty());
        assert!(group.meta.is_none(), "the meta block goes back");
        area.pool.reclaim();
        assert_eq!(area.pool.committed(), block::footprint(META_LEN));
    }

    #[test]
    fn parts_count_against_the_most_entries() {
        let area = Area::with_body_max(60_000);
        let mut group = Group::default();
        let parts = vec![block(&area.pool, b""); ENTRIES_MAX - 1];
        let one = header(1, Path::Live, 0);
        assert_eq!(
            group.push(&area.pool, &area.writer, &[entry(one, &parts)]),
            Ok(())
        );
        let two = &[entry(header(1, Path::Live, 2), &parts[..1]); 2];
        assert_eq!(
            group.push(&area.pool, &area.writer, two),
            Err(Rejected::Record)
        );
        assert_eq!(group.push(&area.pool, &area.writer, &two[..1]), Ok(()));
        assert_eq!(group.writes.len(), ENTRIES_MAX);
    }
}
