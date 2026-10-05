//! The entries of one group commit and the record that holds them.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::iter;

use block::Block;

use crate::entry::{self, Header};
use crate::wal::{Full, Plan, Writer};

/// The entries of one group commit, in append order. [`Closed::clear`] gives it back
/// with its capacity, so two groups that alternate between the handle and the
/// commit task make no heap allocation per commit.
#[derive(Debug, Default)]
pub(crate) struct Group {
    headers: Vec<Header>,
    parts: Vec<Block>,
    table: Vec<u8>,
    /// The bytes of every entry together.
    bytes: usize,
}

/// Why a group did not take an entry. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The record with the entry would be over the layout's maximum body. The
    /// caller closes the group and pushes the entry into the next one.
    Record,
    /// The ring has no room for the record with the entry.
    Ring(Full),
}

impl Group {
    pub(crate) fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// The record body with one more entry of `bytes` bytes: the table and every
    /// entry's bytes.
    fn body_len_with(&self, bytes: usize) -> usize {
        self.headers
            .len()
            .checked_add(1)
            .map(entry::table_len)
            .and_then(|table| table.checked_add(self.bytes))
            .and_then(|body| body.checked_add(bytes))
            .expect("invariant: a body fits in usize")
    }

    /// Adds an entry whose bytes are `parts` together, when the record with it fits
    /// the layout and the ring of `writer`; it sets `header.bytes`.
    ///
    /// # Errors
    ///
    /// [`Rejected`] when the entry does not fit. Nothing changes.
    ///
    /// # Panics
    ///
    /// When the entry alone is over the layout's maximum body, or when `parts` hold
    /// more than `u32::MAX` bytes.
    pub(crate) fn push(
        &mut self,
        writer: &Writer,
        mut header: Header,
        parts: &[Block],
    ) -> Result<(), Rejected> {
        let len = parts.iter().map(|part| part.len()).sum::<usize>();
        let Ok(bytes) = u32::try_from(len) else {
            panic!("invariant: an entry of {len} bytes holds more than u32::MAX");
        };
        header.bytes = bytes;
        let body = self.body_len_with(len);
        if body > writer.body_max() {
            assert!(
                !self.is_empty(),
                "invariant: an entry of {len} bytes is over the maximum body of {} \
                 bytes",
                writer.body_max()
            );
            return Err(Rejected::Record);
        }
        writer.fits(body).map_err(Rejected::Ring)?;
        let Some(total) = self.bytes.checked_add(len) else {
            panic!("invariant: a body fits in usize");
        };
        self.bytes = total;
        self.headers.push(header);
        self.parts.extend(parts.iter().cloned());
        Ok(())
    }

    /// Writes the entry table and plans the record with `writer`.
    ///
    /// # Panics
    ///
    /// When the group is empty.
    pub(crate) fn close(mut self, writer: &mut Writer) -> Closed {
        assert!(
            !self.headers.is_empty(),
            "invariant: an empty group has no record"
        );
        self.table.resize(entry::table_len(self.headers.len()), 0);
        entry::write_table(&self.headers, &mut self.table);
        let table = self.table.as_slice();
        let body = iter::once(table).chain(self.parts.iter().map(|part| &**part));
        let plan = writer.append(body).unwrap_or_else(|full| {
            panic!(
                "invariant: a closed group needs {} bytes of the ring, {} are free",
                full.needed, full.free
            )
        });
        Closed { group: self, plan }
    }
}

/// A group whose record is planned. The record's bytes at `plan.record.place` are
/// `plan.record.header`, then [`table`](Self::table), then each of
/// [`parts`](Self::parts).
#[derive(Debug)]
pub(crate) struct Closed {
    group: Group,
    plan: Plan,
}

impl Closed {
    pub(crate) fn plan(&self) -> Plan {
        self.plan
    }

    /// The entry table.
    pub(crate) fn table(&self) -> &[u8] {
        &self.group.table
    }

    /// The bytes of every entry, in order.
    pub(crate) fn parts(&self) -> &[Block] {
        &self.group.parts
    }

    /// The headers, for the durable tails once the record is synced.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.group.headers
    }

    /// Drops the entries and gives the group back with its capacity.
    pub(crate) fn clear(self) -> Group {
        let mut group = self.group;
        group.headers.clear();
        group.parts.clear();
        group.table.clear();
        group.bytes = 0;
        group
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use block::{Config, Heap, Pool};
    use proptest::prelude::*;
    use types::channel;
    use types::frame::Path;
    use types::hash;
    use types::time::Stamp;

    use crate::entry::table_len;
    use crate::record::HEADER_LEN;
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
            area.write(&plan, [1u32.to_le_bytes().as_slice()]);
            area
        }

        /// Writes the plan's records, with `body` as the record's body.
        fn write<'a>(&mut self, plan: &Plan, body: impl IntoIterator<Item = &'a [u8]>) {
            if let Some(wrap) = plan.wrap {
                let at = index(wrap.place);
                self.bytes[at..at + HEADER_LEN].copy_from_slice(&wrap.header);
            }
            let mut at = index(plan.record.place);
            self.bytes[at..at + HEADER_LEN].copy_from_slice(&plan.record.header);
            at += HEADER_LEN;
            for part in body {
                self.bytes[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
        }

        /// Writes a closed group's record.
        fn commit(&mut self, closed: &Closed) {
            let parts = closed.parts().iter().map(|part| &**part);
            self.write(&closed.plan(), iter::once(closed.table()).chain(parts));
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
            let mut expected: Vec<Vec<Stored>> = Vec::new();
            for entries in groups {
                let mut stored = Vec::new();
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
                    prop_assert_eq!(group.push(&area.writer, header, &blocks), Ok(()));
                    *seq = header.first + u64::from(header.len);
                    stored.push((header, bytes));
                }
                let closed = group.close(&mut area.writer);
                let headers: Vec<Header> = stored.iter().map(|(header, _)| *header).collect();
                prop_assert_eq!(closed.headers(), headers.as_slice());
                area.commit(&closed);
                group = closed.clear();
                prop_assert!(group.is_empty());
                expected.push(stored);
            }

            let bodies = area.walk();
            prop_assert_eq!(bodies.len(), expected.len());
            for (body, stored) in bodies.iter().zip(&expected) {
                let bytes: usize = stored.iter().map(|(_, bytes)| bytes.len()).sum();
                prop_assert_eq!(body.len(), table_len(stored.len()) + bytes);
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
    fn a_closed_group_holds_the_table_and_the_parts() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let mut group = Group::default();
        let parts = [block(&pool, b"abc"), block(&pool, b"de")];
        let first = Header {
            bytes: 3,
            ..header(1, Path::Live, 0)
        };
        let second = Header {
            bytes: 2,
            ..header(2, Path::Backfill, 7)
        };
        group
            .push(&area.writer, first, &parts[..1])
            .expect("the ring has room");
        group
            .push(&area.writer, second, &parts[1..])
            .expect("the ring has room");
        let closed = group.close(&mut area.writer);
        let mut table = vec![0; table_len(2)];
        entry::write_table(&[first, second], &mut table);
        assert_eq!(closed.table(), table);
        let parts: Vec<&[u8]> = closed.parts().iter().map(|part| &**part).collect();
        assert_eq!(parts, [b"abc".as_slice(), b"de"]);
        assert_eq!(closed.headers(), [first, second]);
    }

    #[test]
    fn push_sets_the_bytes_of_the_header_from_its_parts() {
        let pool = pool(1 << 16);
        let area = Area::new();
        let mut group = Group::default();
        group
            .push(
                &area.writer,
                header(1, Path::Live, 0),
                &[block(&pool, b"abcd"), block(&pool, b"")],
            )
            .expect("the ring has room");
        group
            .push(&area.writer, header(1, Path::Live, 2), &[])
            .expect("the ring has room");
        let bytes: Vec<u32> = group.headers.iter().map(|header| header.bytes).collect();
        assert_eq!(bytes, [4, 0]);
        assert_eq!(group.body_len_with(10), table_len(3) + 14);
    }

    #[test]
    fn a_group_with_an_entry_of_no_bytes_is_not_empty() {
        let area = Area::new();
        let mut group = Group::default();
        group
            .push(&area.writer, header(1, Path::Live, 0), &[])
            .expect("the ring has room");
        assert!(!group.is_empty());
    }

    #[test]
    fn an_entry_over_the_record_asks_for_a_close() {
        let pool = pool(1 << 16);
        let area = Area::new();
        let mut group = Group::default();
        let half = (BODY_MAX - table_len(2)) / 2 + 1;
        let big = block(&pool, &vec![7; half]);
        group
            .push(
                &area.writer,
                header(1, Path::Live, 0),
                std::slice::from_ref(&big),
            )
            .expect("the ring has room");
        let rejected = group.push(&area.writer, header(1, Path::Live, 2), &[big]);
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        assert_eq!(group.bytes, half);
    }

    #[test]
    fn an_entry_the_ring_has_no_room_for_changes_nothing() {
        let pool = pool(1 << 20);
        let mut area = Area::new();
        let mut group = Group::default();
        let part = block(&pool, &vec![7; BODY_MAX - table_len(1)]);
        let entry = header(1, Path::Live, 0);
        let rejected = loop {
            match group.push(&area.writer, entry, std::slice::from_ref(&part)) {
                Ok(()) => {
                    let closed = group.close(&mut area.writer);
                    area.commit(&closed);
                    group = closed.clear();
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
    }

    #[test]
    fn clear_keeps_the_capacity() {
        let pool = pool(1 << 16);
        let mut area = Area::new();
        let mut group = Group::default();
        for first in 0..3 {
            group
                .push(
                    &area.writer,
                    header(1, Path::Live, 2 * first),
                    &[block(&pool, b"xy")],
                )
                .expect("the ring has room");
        }
        let closed = group.close(&mut area.writer);
        let capacity = |group: &Group| {
            (
                group.headers.capacity(),
                group.parts.capacity(),
                group.table.capacity(),
            )
        };
        let before = capacity(&closed.group);
        let group = closed.clear();
        assert!(group.is_empty());
        assert_eq!(group.body_len_with(0), table_len(1));
        assert_eq!(capacity(&group), before);
    }

    #[test]
    #[should_panic(expected = "invariant: an empty group has no record")]
    fn close_of_an_empty_group_is_a_broken_invariant() {
        let mut area = Area::new();
        let _closed = Group::default().close(&mut area.writer);
    }

    #[test]
    #[should_panic(
        expected = "invariant: an entry of 8138 bytes is over the maximum body of 8192 \
                    bytes"
    )]
    fn an_entry_alone_over_the_record_is_a_broken_invariant() {
        let pool = pool(1 << 16);
        let area = Area::new();
        let part = block(&pool, &vec![7; BODY_MAX - table_len(1) + 1]);
        let _rejected =
            Group::default().push(&area.writer, header(1, Path::Live, 0), &[part]);
    }
}
