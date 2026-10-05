//! The entries of one group commit and the record that holds them.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::iter;

use block::{Block, Pool};

use crate::entry::{self, Header};
use crate::record::HEADER_LEN;
use crate::wal::{Plan, Writer};

/// The entries of one group commit, in append order. Cleared, it keeps its capacity,
/// so two groups that alternate between the handle and the commit task make no heap
/// allocation per commit.
#[derive(Debug, Default)]
pub(crate) struct Group {
    headers: Vec<Header>,
    parts: Vec<Block>,
    /// The bytes of every entry together.
    bytes: usize,
    plan: Option<Plan>,
}

impl Group {
    pub(crate) fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// The record body with one more entry of `bytes` bytes: the table and every
    /// entry's bytes. The caller holds it under the layout's `body_max` and checks
    /// it with [`Writer::fits`] before [`push`](Self::push).
    ///
    /// # Panics
    ///
    /// When the body would be over `usize::MAX` bytes.
    pub(crate) fn body_len_with(&self, bytes: usize) -> usize {
        self.headers
            .len()
            .checked_add(1)
            .map(entry::table_len)
            .and_then(|table| table.checked_add(self.bytes))
            .and_then(|body| body.checked_add(bytes))
            .expect("invariant: a body fits in usize")
    }

    /// Adds an entry whose bytes are `parts` together; it sets `header.bytes`.
    ///
    /// # Panics
    ///
    /// After [`close`](Self::close), before [`clear`](Self::clear), or when `parts`
    /// hold more than `u32::MAX` bytes.
    pub(crate) fn push(&mut self, mut header: Header, parts: &[Block]) {
        assert!(
            self.plan.is_none(),
            "invariant: a closed group takes no entry"
        );
        let len = parts.iter().map(|part| part.len()).sum::<usize>();
        header.bytes = u32::try_from(len)
            .expect("invariant: an entry holds at most u32::MAX bytes");
        self.bytes = self
            .bytes
            .checked_add(len)
            .expect("invariant: a body fits in usize");
        self.headers.push(header);
        self.parts.extend(parts.iter().cloned());
    }

    /// Plans the record with `writer` and puts the block that holds the record
    /// header and the table in front of [`parts`](Self::parts). The writes are the
    /// plan's, with the parts as the record's bytes.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when `pool` has no block for the table. Nothing changes.
    ///
    /// # Panics
    ///
    /// When the group is empty or closed, or when the record does not fit the ring:
    /// each push passed [`Writer::fits`], and the head did not move since.
    pub(crate) fn close(
        &mut self,
        writer: &mut Writer,
        pool: &Pool,
    ) -> Result<Plan, block::Error> {
        assert!(
            !self.headers.is_empty(),
            "invariant: an empty group has no record"
        );
        assert!(self.plan.is_none(), "invariant: a group closes once");
        let table = entry::table_len(self.headers.len());
        let len = HEADER_LEN
            .checked_add(table)
            .expect("invariant: a body fits in usize");
        let mut head = pool.alloc(len)?;
        let (header, table) = head.split_at_mut(HEADER_LEN);
        entry::write_table(&self.headers, table);
        let body = iter::once(&*table).chain(self.parts.iter().map(|part| &**part));
        let plan = writer.append(body).unwrap_or_else(|full| {
            panic!(
                "invariant: a closed group needs {} bytes of the ring, {} are free",
                full.needed, full.free
            )
        });
        header.copy_from_slice(&plan.record.header);
        self.parts.insert(0, head.freeze());
        self.plan = Some(plan);
        Ok(plan)
    }

    /// The headers, for the durable tails once the record is synced.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.headers
    }

    /// The blocks to write back to back at the record's place.
    ///
    /// # Panics
    ///
    /// Before [`close`](Self::close).
    pub(crate) fn parts(&self) -> &[Block] {
        assert!(
            self.plan.is_some(),
            "invariant: an open group has no record"
        );
        &self.parts
    }

    /// Drops the entries and the plan, and keeps the capacity.
    pub(crate) fn clear(&mut self) {
        self.headers.clear();
        self.parts.clear();
        self.bytes = 0;
        self.plan = None;
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
    use crate::wal::{Cursor, Layout, Position, Step, Window};

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

    fn start() -> Position {
        Position::new(0, CHAIN).expect("offset 0 is aligned")
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    /// An area in memory and the writer that continues it, as a ring just made and
    /// opened: it holds one restart record.
    struct Area {
        bytes: Vec<u8>,
        writer: Writer,
    }

    impl Area {
        fn new() -> Self {
            let bytes = vec![0; index(AREA)];
            let layout = Layout::new(AREA, BODY_MAX).expect("the sizes make a ring");
            let mut cursor = Cursor::new(layout, start());
            let Window { place, len } = cursor.window();
            let step = cursor.next(&bytes[index(place)..index(place + len)]);
            assert_eq!(step, Ok(Step::End), "a zeroed area ends at once");
            let (writer, plan) = cursor.writer(start(), 1).expect("the ring is empty");
            let mut area = Self { bytes, writer };
            area.write(&plan, [plan.record.header.as_slice(), &1u32.to_le_bytes()]);
            area
        }

        fn write<'a>(
            &mut self,
            plan: &Plan,
            parts: impl IntoIterator<Item = &'a [u8]>,
        ) {
            if let Some(wrap) = plan.wrap {
                let at = index(wrap.place);
                self.bytes[at..at + HEADER_LEN].copy_from_slice(&wrap.header);
            }
            let mut at = index(plan.record.place);
            for part in parts {
                self.bytes[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
        }

        /// The data bodies, as recovery reads them.
        fn walk(&self) -> Vec<Vec<u8>> {
            let layout = Layout::new(AREA, BODY_MAX).expect("the sizes make a ring");
            let mut cursor = Cursor::new(layout, start());
            let mut bodies = Vec::new();
            loop {
                let Window { place, len } = cursor.window();
                let window = &self.bytes[index(place)..index(place + len)];
                match cursor
                    .next(window)
                    .expect("the ring holds what was written")
                {
                    Step::Data(body) => bodies.push(body.to_vec()),
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
            let pool = pool(1 << 20);
            let mut area = Area::new();
            let mut seqs: hash::Map<(u128, Path), u64> = hash::Map::default();
            let mut group = Group::default();
            let mut expected: Vec<(Vec<Stored>, usize)> = Vec::new();
            for entries in groups {
                let mut stored = Vec::new();
                let mut body_len = 0;
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
                        next.parts.iter().map(|part| block(&pool, part)).collect();
                    body_len = group.body_len_with(bytes.len());
                    prop_assert!(body_len <= BODY_MAX);
                    prop_assert_eq!(area.writer.fits(body_len), Ok(()));
                    group.push(header, &blocks);
                    *seq = header.first + u64::from(header.len);
                    stored.push((header, bytes));
                }
                let plan = group.close(&mut area.writer, &pool).expect("the pool has room");
                let headers: Vec<Header> = stored.iter().map(|(header, _)| *header).collect();
                prop_assert_eq!(group.headers(), headers.as_slice());
                area.write(&plan, group.parts().iter().map(|part| &**part));
                group.clear();
                prop_assert!(group.is_empty());
                expected.push((stored, body_len));
            }

            let bodies = area.walk();
            prop_assert_eq!(bodies.len(), expected.len());
            for (body, (stored, body_len)) in bodies.iter().zip(&expected) {
                prop_assert_eq!(body.len(), *body_len);
                let entries: Result<Vec<(Header, &[u8])>, _> =
                    entry::parse(body).expect("the table is whole").collect();
                let entries = entries.expect("every entry is whole");
                let stored: Vec<(Header, &[u8])> =
                    stored.iter().map(|(header, bytes)| (*header, bytes.as_slice())).collect();
                prop_assert_eq!(entries, stored);
            }
        }
    }

    #[test]
    fn the_first_part_holds_the_record_header_and_the_table() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let mut group = Group::default();
        let parts = [block(&pool, b"abc"), block(&pool, b"de")];
        group.push(header(1, Path::Live, 0), &parts[..1]);
        group.push(header(2, Path::Backfill, 7), &parts[1..]);
        let plan = group
            .close(&mut area.writer, &pool)
            .expect("the pool has room");
        let first = Header {
            bytes: 3,
            ..header(1, Path::Live, 0)
        };
        let second = Header {
            bytes: 2,
            ..header(2, Path::Backfill, 7)
        };
        let mut table = vec![0; table_len(2)];
        entry::write_table(&[first, second], &mut table);
        let parts: Vec<&[u8]> = group.parts().iter().map(|part| &**part).collect();
        let head = [plan.record.header.as_slice(), &table].concat();
        assert_eq!(parts, [head.as_slice(), b"abc", b"de"]);
        assert_eq!(group.headers(), [first, second]);
    }

    #[test]
    fn push_sets_the_bytes_of_the_header_from_its_parts() {
        let pool = pool(1 << 16);
        let mut group = Group::default();
        group.push(
            header(1, Path::Live, 0),
            &[block(&pool, b"abcd"), block(&pool, b"")],
        );
        group.push(header(1, Path::Live, 2), &[]);
        let bytes: Vec<u32> =
            group.headers().iter().map(|header| header.bytes).collect();
        assert_eq!(bytes, [4, 0]);
        assert_eq!(group.body_len_with(10), table_len(3) + 14);
    }

    #[test]
    fn close_without_a_block_for_the_table_changes_nothing() {
        let pool = pool(4096);
        let mut area = Area::new();
        let mut group = Group::default();
        group.push(header(1, Path::Live, 0), &[block(&pool, b"abc")]);
        let mut kept = Vec::new();
        while let Ok(block) = pool.alloc(64) {
            kept.push(block);
        }
        let head = area.writer.head();
        let error = block::Error::Exhausted {
            requested: HEADER_LEN + table_len(1),
            available: 4096 - pool.committed(),
        };
        assert_eq!(group.close(&mut area.writer, &pool), Err(error));
        assert_eq!(area.writer.head(), head);
        assert_eq!(group.headers().len(), 1);
        assert!(group.plan.is_none());
        drop(kept);
        pool.reclaim();
        group
            .close(&mut area.writer, &pool)
            .expect("the pool has room again");
    }

    #[test]
    fn clear_keeps_the_capacity() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let mut group = Group::default();
        for first in 0..3 {
            group.push(header(1, Path::Live, 2 * first), &[block(&pool, b"xy")]);
        }
        group
            .close(&mut area.writer, &pool)
            .expect("the pool has room");
        let (headers, parts) = (group.headers.capacity(), group.parts.capacity());
        group.clear();
        assert!(group.is_empty());
        assert!(group.plan.is_none());
        assert_eq!(group.body_len_with(0), table_len(1));
        assert_eq!(
            (group.headers.capacity(), group.parts.capacity()),
            (headers, parts)
        );
    }

    #[test]
    #[should_panic(expected = "invariant: an empty group has no record")]
    fn close_of_an_empty_group_is_a_broken_invariant() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let _closed = Group::default().close(&mut area.writer, &pool);
    }

    #[test]
    #[should_panic(expected = "invariant: a closed group takes no entry")]
    fn push_after_close_is_a_broken_invariant() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let mut group = Group::default();
        group.push(header(1, Path::Live, 0), &[]);
        group
            .close(&mut area.writer, &pool)
            .expect("the pool has room");
        group.push(header(1, Path::Live, 2), &[]);
    }

    #[test]
    #[should_panic(expected = "invariant: an open group has no record")]
    fn parts_before_close_is_a_broken_invariant() {
        let _parts = Group::default().parts();
    }
}
