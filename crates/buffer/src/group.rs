//! The entries of one group commit and the record that holds them.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::iter;
use std::slice;

use block::{Block, Pool, Unique};
use types::channel::Slot;

use crate::entry::{self, Entry, Header, Kind};
use crate::record;
use crate::wal::{Ends, Full, Layout, Limit, Plan, Writer};

/// Bytes of the block that holds a record header and the largest entry table: one
/// block of the pool's 64 KiB class.
pub(crate) const META_LEN: usize = record::HEADER_LEN + entry::TABLE_MAX;
const _: () = assert!(META_LEN <= 1 << 16, "the table fits one 64 KiB block");

/// The most bytes of parts, joined, that one entry may have on `pool`: one block of
/// `pool` holds them, and the group's two blocks and the entry, in any split into two
/// parts, fit its budget at once.
///
/// # Errors
///
/// The least budget that holds an entry of [`Layout::ENTRY_MAX_MIN`] bytes, when
/// the budget of `pool` does not.
pub(crate) fn block_max(pool: &Pool) -> Result<usize, usize> {
    let (budget, largest) = (pool.budget(), pool.largest());
    let floor = Layout::ENTRY_MAX_MIN;
    let starts = class_starts(largest.max(floor));
    let min = need(&starts, floor);
    if min > budget {
        return Err(min);
    }
    let (mut low, mut high) = (floor, largest);
    while low < high {
        let mid = low.midpoint(high).saturating_add(1);
        if need(&starts, mid) <= budget {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    Ok(low)
}

/// The first length of each size class of the pool, up to `largest`.
fn class_starts(largest: usize) -> Vec<usize> {
    let mut starts = vec![0];
    let mut start = 0;
    loop {
        let footprint = block::footprint(start);
        let (mut low, mut high) = (start, largest);
        while low < high {
            let mid = low.midpoint(high).saturating_add(1);
            if block::footprint(mid) == footprint {
                low = mid;
            } else {
                high = mid.saturating_sub(1);
            }
        }
        if low >= largest {
            return starts;
        }
        start = low.saturating_add(1);
        starts.push(start);
    }
}

/// Budget bytes that an entry of `len` bytes needs with the group's two blocks, in
/// the split into two parts that takes the most. `starts` holds the class starts up
/// to at least `len`: in a class, the first length leaves the most to the other part.
fn need(starts: &[usize], len: usize) -> usize {
    let group =
        block::footprint(META_LEN).saturating_add(block::footprint(record::HEADER_LEN));
    let parts = starts
        .iter()
        .take_while(|&&first| first <= len)
        .map(|&first| {
            block::footprint(first)
                .saturating_add(block::footprint(len.saturating_sub(first)))
        })
        .max()
        .expect("invariant: the class of 0 starts at 0");
    group.saturating_add(parts)
}

/// The entries of one group commit, in append order. The first fit takes the block
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
    /// The batch alone is over a [`Limit`], so this buffer never takes it.
    Large(Limit),
    /// The record with the batch and the group's entries would be over a
    /// [`Limit`] or past the room in the ring. The caller closes the group and
    /// pushes the batch into the next one.
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

    /// Checks that the group can take every entry of `batch`: the record with them
    /// fits the layout and the ring of `writer`, and each entry is at most
    /// `block_max`. When the group has no blocks, it takes them from `pool`. Counts
    /// the bytes of the batch in the group, so the caller then gives each entry of
    /// `batch` to [`take`](Self::take), in order.
    ///
    /// # Errors
    ///
    /// [`Rejected`] when the batch does not fit or the pool has no block. Nothing
    /// changes.
    pub(crate) fn fit(
        &mut self,
        pool: &Pool,
        block_max: usize,
        writer: &Writer,
        batch: &[Entry],
    ) -> Result<(), Rejected> {
        let count = batch.len();
        let start = self.headers.len();
        if count == 0 {
            return Ok(());
        }
        let parts = batch.iter().map(|entry| entry.parts.len()).sum::<usize>();
        let len = batch.iter().map(|entry| entry.parts.bytes()).sum::<usize>();
        let layout = writer.layout();
        layout.check(count, parts, len).map_err(Rejected::Large)?;
        let max = Layout::ENTRY_MAX_MIN;
        let mut tagged = None;
        for entry in batch {
            let len = entry.parts.bytes();
            if len > block_max {
                return Err(Rejected::Large(Limit::Block {
                    len,
                    max: block_max,
                }));
            }
            if tagged.is_none() && entry.tag != 0 && len > max {
                tagged = Some(Limit::Tagged { len, max });
            }
        }
        if let Some(limit) = tagged {
            return Err(Rejected::Large(limit));
        }
        layout
            .check(
                start.saturating_add(count),
                self.writes.len().saturating_add(parts),
                self.bytes.saturating_add(len),
            )
            .map_err(|_over: Limit| Rejected::Record)?;
        let body = self.body_len_with(count, len);
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
        self.bytes = self
            .bytes
            .checked_add(len)
            .expect("invariant: a body fits in usize");
        Ok(())
    }

    /// Takes `entry` with its header of `kind`, after a [`fit`](Self::fit) of its
    /// batch.
    pub(crate) fn take(&mut self, entry: Entry, kind: Kind) {
        self.headers.push(entry.header(kind));
        self.slots.push(entry.slot);
        self.writes.extend(entry.parts);
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
    /// it. The next record follows the chain value of [`Sealed::ends`].
    pub(crate) fn seal(self, chain: u32) -> Sealed {
        let Self {
            mut group,
            plan,
            mut meta,
            wrap,
            start,
        } = self;
        let (_, tail) = meta.split_at_mut(start);
        let (header, table) = tail.split_at_mut(record::HEADER_LEN);
        let parts = group.writes.iter().map(|part| &**part);
        let body = iter::once(&*table).chain(parts);
        let (sealed, ends) = plan.seal(chain, body);
        header.copy_from_slice(&sealed.record.header);
        group.writes.insert(0, meta.freeze().skip(start));
        let wrap = sealed
            .wrap
            .map(|write| (write.place, end_block(wrap, &write.header)));
        Sealed {
            group,
            plan,
            wrap,
            ends,
        }
    }
}

/// `header` at the end of `block`, as a record of no body: the wrap header of a
/// record that wraps.
///
/// # Panics
///
/// When `block` is shorter than a record header.
pub(crate) fn end_block(mut block: Unique, header: &[u8; record::HEADER_LEN]) -> Block {
    let start = block
        .len()
        .checked_sub(record::HEADER_LEN)
        .unwrap_or_else(|| {
            panic!(
                "invariant: a block of {} bytes holds no wrap header",
                block.len()
            )
        });
    let (_, end) = block.split_at_mut(start);
    end.copy_from_slice(header);
    block.freeze().skip(start)
}

/// Takes from the front of `writes` the blocks of an entry of `bytes` bytes. Each
/// entry has whole blocks, so the bytes split at a block end.
fn parts<'a>(writes: &mut &'a [Block], bytes: u32) -> &'a [Block] {
    let bytes = usize::try_from(bytes).expect("invariant: an entry fits in usize");
    let mut sum = 0_usize;
    let count = writes
        .iter()
        .take_while(|part| {
            let before = sum;
            sum = sum.saturating_add(part.len());
            before < bytes
        })
        .count();
    let (parts, rest) = writes.split_at(count);
    *writes = rest;
    parts
}

/// A group whose record is complete: each of its [`writes`](Self::writes) goes at
/// its place in the area.
#[derive(Debug)]
pub(crate) struct Sealed {
    group: Group,
    plan: Plan,
    wrap: Option<(u64, Block)>,
    ends: Ends,
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
            .chain(iter::once((self.plan.place, &*self.group.writes)))
    }

    /// The offset of the record.
    pub(crate) fn offset(&self) -> u64 {
        self.plan.offset
    }

    /// The boundaries after the record's writes, for the writer once the record is
    /// synced.
    pub(crate) fn ends(&self) -> Ends {
        self.ends
    }

    /// The headers, for the durable tails once the record is synced.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.group.headers
    }

    /// The slot of each header, in the same order.
    pub(crate) fn slots(&self) -> &[Slot] {
        &self.group.slots
    }

    /// Each entry with a nonzero tag, in order: its slot, its header, and its parts.
    pub(crate) fn tagged(&self) -> impl Iterator<Item = (Slot, &Header, &[Block])> {
        let group = &self.group;
        let headers = if group.headers.iter().any(|header| header.tag != 0) {
            group.headers.as_slice()
        } else {
            &[]
        };
        // `seal` put the record header and the table at `writes[0]`.
        let mut writes = group.writes.get(1..).unwrap_or_default();
        group
            .slots
            .iter()
            .zip(headers)
            .filter_map(move |(&slot, header)| {
                let parts = parts(&mut writes, header.bytes);
                (header.tag != 0).then_some((slot, header, parts))
            })
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
    use std::num::{NonZeroU32, NonZeroU64};
    use std::ops::Range;

    use super::*;
    use block::{Config, Heap};
    use proptest::prelude::*;
    use types::channel;
    use types::frame::Path;
    use types::hash;
    use types::time::Stamp;

    use crate::entry::{ENTRIES_MAX, Parts, Samples, table_len};
    use crate::record::HEADER_LEN;
    use crate::wal::{self, Cursor, Position, Step, Window};
    use types::log::Mark;

    const BLOCKS: u64 = 32;
    const AREA: u64 = BLOCKS * 4096;
    const BODY_MAX: usize = 8192;
    const CHAIN: u32 = 0x5EED_0001;

    fn pool(budget: usize) -> Pool {
        let budget = u64::try_from(budget).expect("a usize fits in a u64");
        let config = Config::new(budget).expect("the budget fits");
        Pool::new(config.clone(), Heap::new(config.reservation()))
    }

    fn block(pool: &Pool, bytes: &[u8]) -> Block {
        pool.copy(bytes).expect("the pool has room")
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
            stored_at: Stamp::from_nanos(5),
            kind: Kind::Samples(Samples {
                len: NonZeroU32::new(2).expect("not 0"),
                last: Some(Stamp::from_nanos(9)),
            }),
            tag: 0,
            bytes: 99,
        }
    }

    /// Blocks of `bytes`, at most two, as the parts of one entry.
    fn parts(pool: &Pool, bytes: &[&[u8]]) -> Parts {
        match bytes {
            [] => Parts::default(),
            [one] => Parts::from(block(pool, one)),
            [one, two] => Parts::from([block(pool, one), block(pool, two)]),
            _ => panic!("an entry holds at most two parts"),
        }
    }

    /// The entry that `push` makes `header` from; `bytes` comes from the parts.
    fn entry(header: Header, parts: Parts) -> Entry {
        Entry {
            index: header.index,
            slot: slot(header.index.as_u128()),
            path: header.path,
            first: header.first,
            samples: header.samples(),
            stored_at: header.stored_at,
            tag: header.tag,
            parts,
        }
    }

    /// Fits `batch` in `group`, then takes each of its entries, as the buffer does,
    /// with no count: an entry with no samples counts 1. Gives the positions of the
    /// entries it took. `batch` keeps its entries on an error.
    fn push(
        group: &mut Group,
        pool: &Pool,
        block_max: usize,
        writer: &Writer,
        batch: &mut Vec<Entry>,
    ) -> Result<Range<usize>, Rejected> {
        group.fit(pool, block_max, writer, batch)?;
        let start = group.headers.len();
        for entry in batch.drain(..) {
            let kind = entry
                .samples
                .map_or(Kind::Record(NonZeroU64::MIN), Kind::Samples);
            group.take(entry, kind);
        }
        Ok(start..group.headers.len())
    }

    fn start() -> Position {
        Position::new(0, CHAIN).expect("offset 0 is aligned")
    }

    fn index(value: u64) -> usize {
        usize::try_from(value).expect("an offset in the test area fits in usize")
    }

    /// The group's blocks and the costliest split of `len` into two parts, over
    /// every split.
    fn need_of_every_split(len: usize) -> usize {
        let group = block::footprint(META_LEN) + block::footprint(HEADER_LEN);
        let parts = (0..=len)
            .map(|first| block::footprint(first) + block::footprint(len - first))
            .max()
            .expect("a split at 0");
        group + parts
    }

    #[test]
    fn need_takes_the_costliest_split() {
        let starts = class_starts(1 << 20);
        for len in (0..=2_000).chain([4_095, 4_096, 4_097, 61_441, 70_000]) {
            assert_eq!(need(&starts, len), need_of_every_split(len), "len {len}");
        }
    }

    #[test]
    fn class_starts_are_the_lengths_where_a_footprint_grows() {
        let largest = 1 << 17;
        let grows = (1..=largest)
            .filter(|&len| block::footprint(len) > block::footprint(len - 1));
        let expected: Vec<usize> = iter::once(0).chain(grows).collect();
        assert_eq!(class_starts(largest), expected);
    }

    #[test]
    fn no_block_max_below_the_floor() {
        let min = need_of_every_split(Layout::ENTRY_MAX_MIN);
        assert_eq!(block_max(&pool(min - 1)), Err(min));
        assert_eq!(block_max(&pool(min)), Ok(4_096));
        assert!(pool(min).largest() >= META_LEN);
        assert_eq!(block_max(&pool(1_000)), Err(min));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// `block_max` is the largest length whose need fits the budget, and a pool
        /// that it accepts holds a block of `META_LEN` bytes.
        #[test]
        fn block_max_is_the_most_that_fits(budget in 62_272usize..=1 << 22) {
            let pool = pool(budget);
            let largest = pool.largest();
            let max = block_max(&pool).expect("the floor fits");
            let starts = class_starts(largest);
            prop_assert!(largest >= META_LEN);
            prop_assert!(max <= largest);
            prop_assert!(need(&starts, max) <= budget);
            prop_assert!(max == largest || need(&starts, max + 1) > budget);
        }
    }

    /// The smallest body a header can open holds one entry of 4032 bytes.
    #[test]
    fn a_ring_that_a_header_opens_holds_one_entry_of_4032_bytes() {
        use crate::header;
        let small = Layout::new(AREA, 4087).expect("the sizes make a ring");
        let block = header::Header::new(small, CHAIN).encode();
        let opened = header::Header::decode(&block, &[0; 4096]).expect("a whole block");
        let bytes = vec![0; index(AREA)];
        let mut cursor = Cursor::new(opened.layout, opened.tail, 1 << 16);
        let Window { place, len } = cursor.window();
        let step = cursor.next(&bytes[index(place)..index(place) + len]);
        assert_eq!(step, Ok(Step::End));
        let mut writer = cursor.writer();
        writer.restart(1).expect("the ring is empty");
        let memory = pool(1 << 20);
        let parts = parts(&memory, &[&[7; 4032]]);
        let entry = entry(header(1, Path::Live, 0), parts);
        let pushed = push(
            &mut Group::default(),
            &memory,
            block_max(&memory).expect("the floor fits"),
            &writer,
            &mut vec![entry],
        );
        assert_eq!(pushed, Ok(0..1));
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
            Self::with(Layout::new(AREA, BODY_MAX).expect("the sizes make a ring"))
        }

        /// The smallest area whose records hold a body of at most `body_max` bytes.
        fn with_body_max(body_max: usize) -> Self {
            let min = Layout::fit(0, body_max)
                .expect_err("no ring in no bytes")
                .min;
            Self::with(Layout::fit(min, body_max).expect("the least length fits"))
        }

        fn with(layout: Layout) -> Self {
            let bytes = vec![0; index(layout.area())];
            let mut cursor = Cursor::new(layout, start(), 1 << 16);
            let Window { place, len } = cursor.window();
            let step = cursor.next(&bytes[index(place)..index(place) + len]);
            assert_eq!(step, Ok(Step::End), "a zeroed area ends at once");
            let mut writer = cursor.writer();
            let sealed = writer.restart(1).expect("the ring is empty");
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
            let sealed = closed.seal(self.chain);
            let chain = sealed.ends().record.chain();
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

        fn block_max(&self) -> usize {
            block_max(&self.pool).expect("the floor fits")
        }

        /// Fits and takes the entry of `header`, of the kind of `header`, as the
        /// buffer does.
        fn push(&mut self, group: &mut Group, header: Header, parts: Parts) {
            let entry = entry(header, parts);
            group
                .fit(
                    &self.pool,
                    self.block_max(),
                    &self.writer,
                    slice::from_ref(&entry),
                )
                .expect("the ring has room");
            group.take(entry, header.kind);
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
                    Step::Carry(_) => panic!("the group writes no carry record"),
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
                last: last.filter(|_| len != 0),
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
            let mut marks: hash::Map<(u128, Path), Mark> = hash::Map::default();
            let mut group = Group::default();
            let mut expected: Vec<Vec<Stored>> = Vec::new();
            for entries in groups {
                let mut stored = Vec::new();
                let mut slots = Vec::new();
                for next in entries {
                    let mark = marks.entry((next.index, next.path)).or_insert(Mark::at(0));
                    let bytes = next.parts.concat();
                    let first = mark.seq + next.skip;
                    *mark = crate::log::after(*mark, first, next.len);
                    let header = Header {
                        index: key(next.index),
                        path: next.path,
                        first,
                        stored_at: Stamp::from_nanos(5),
                        kind: match NonZeroU32::new(next.len) {
                            Some(len) => Kind::Samples(Samples {
                                len,
                                last: next.last.map(Stamp::from_nanos),
                            }),
                            None => Kind::Record(
                                NonZeroU64::new(mark.given).expect("counts from 1"),
                            ),
                        },
                        tag: next.tag,
                        bytes: u32::try_from(bytes.len()).expect("under 300 bytes"),
                    };
                    let slices: Vec<&[u8]> = next.parts.iter().map(Vec::as_slice).collect();
                    area.push(&mut group, header, parts(&area.pool, &slices));
                    stored.push((header, bytes));
                    slots.push(slot(next.index));
                }
                let closed = group.close(&mut area.writer);
                let sealed = area.commit(closed);
                let headers: Vec<Header> = stored.iter().map(|(header, _)| *header).collect();
                prop_assert_eq!(sealed.headers(), headers.as_slice());
                prop_assert_eq!(sealed.slots(), slots.as_slice());
                let tagged: Vec<(Slot, Header, Vec<u8>)> = sealed
                    .tagged()
                    .map(|(slot, header, parts)| {
                        (slot, *header, parts.iter().flat_map(|part| part.to_vec()).collect())
                    })
                    .collect();
                let wanted: Vec<(Slot, Header, Vec<u8>)> = slots
                    .iter()
                    .zip(&stored)
                    .filter(|(_, (header, _))| header.tag != 0)
                    .map(|(&slot, (header, bytes))| (slot, *header, bytes.clone()))
                    .collect();
                prop_assert_eq!(tagged, wanted);
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
        let first = Header {
            bytes: 3,
            ..header(1, Path::Live, 0)
        };
        let second = Header {
            bytes: 2,
            ..header(2, Path::Backfill, 7)
        };
        area.push(&mut group, first, parts(&area.pool, &[b"abc"]));
        area.push(&mut group, second, parts(&area.pool, &[b"de"]));
        let sealed = group.close(&mut area.writer).seal(area.chain);
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
            area.push(&mut first, header(1, Path::Live, 0), a.into());
            area.push(&mut second, header(2, Path::Live, 0), b.into());
            (
                first.close(&mut area.writer),
                second.close(&mut area.writer),
            )
        };
        let mut right = Area::new();
        let (first, second) = close_two(&mut right);
        let a = first.seal(right.chain);
        let b = second.seal(a.ends().record.chain());
        right.put(&a);
        right.put(&b);
        assert_eq!(right.walk().len(), 2);
        let mut wrong = Area::new();
        let (first, second) = close_two(&mut wrong);
        let a = first.seal(wrong.chain);
        let b = second.seal(wrong.chain);
        wrong.put(&a);
        wrong.put(&b);
        assert_eq!(
            wrong.walk().len(),
            1,
            "the second is sealed from the wrong chain"
        );
    }

    /// Ten records of three blocks follow the restart block, with the tail past the
    /// second record, so a record of two blocks does not fit in the last block and
    /// wraps.
    #[test]
    fn a_closed_group_holds_the_wrap_header_when_the_record_wraps() {
        let mut area = Area::new();
        let mut group = Group::default();
        let mut tail = start();
        for first in 0..10 {
            let part = block(&area.pool, &vec![7; BODY_MAX - table_len(1)]);
            area.push(&mut group, header(1, Path::Live, 2 * first), part.into());
            let closed = group.close(&mut area.writer);
            let sealed = area.commit(closed);
            area.writer.synced(sealed.ends());
            if first == 1 {
                tail = sealed.ends().record;
                area.writer.release(tail.offset());
            }
            group = sealed.clear();
        }
        assert_eq!(area.writer.head(), 31 * 4096);
        assert_eq!(tail.offset(), 7 * 4096);
        let part = block(&area.pool, &[7; 4096]);
        area.push(&mut group, header(1, Path::Live, 100), part.into());
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
        assert_eq!(sealed.offset(), 32 * 4096, "the offset is past the wrap");
        assert_eq!(sealed.ends().record.offset(), 34 * 4096);
        let bodies = area.walk_from(tail);
        assert_eq!(bodies.len(), 9);
        assert_eq!(bodies[8].len(), table_len(1) + 4096);
    }

    /// A part given as `Some` or `None` writes like one given as a block or as
    /// no block.
    #[test]
    fn push_takes_an_optional_part() {
        let mut area = Area::new();
        let mut group = Group::default();
        let some = Parts::from(Some(block(&area.pool, b"xyz")));
        let none = Parts::from(None);
        area.push(&mut group, header(1, Path::Live, 0), some);
        area.push(&mut group, header(1, Path::Live, 2), none);
        let bytes: Vec<u32> = group.headers.iter().map(|header| header.bytes).collect();
        assert_eq!(bytes, [3, 0]);
        let sealed = group.close(&mut area.writer).seal(area.chain);
        let parts: Vec<&[u8]> = sealed
            .writes()
            .flat_map(|(_, blocks)| blocks.iter().map(|part| &**part))
            .skip(1)
            .collect();
        assert_eq!(parts, [b"xyz".as_slice()]);
    }

    #[test]
    fn push_sets_the_bytes_of_the_header_from_its_parts() {
        let mut area = Area::new();
        let mut group = Group::default();
        area.push(
            &mut group,
            header(1, Path::Live, 0),
            parts(&area.pool, &[b"abcd", b""]),
        );
        area.push(&mut group, header(1, Path::Live, 2), Parts::default());
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
        area.push(&mut group, header(1, Path::Live, 0), Parts::default());
        assert!(!group.is_empty());
    }

    #[test]
    fn an_entry_over_the_record_asks_for_a_close() {
        let mut area = Area::new();
        let mut group = Group::default();
        let half = (BODY_MAX - table_len(2)) / 2 + 1;
        let big = block(&area.pool, &vec![7; half]);
        area.push(&mut group, header(1, Path::Live, 0), big.clone().into());
        let rejected = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut vec![entry(header(1, Path::Live, 2), big.into())],
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
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut vec![entry(header(1, Path::Live, first), Parts::default())],
            )
        };
        for first in 0..ENTRIES_MAX {
            assert_eq!(
                push(u64::try_from(first).expect("small")),
                Ok(first..first + 1)
            );
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
            let pushed = push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut vec![entry(one, part.clone().into())],
            );
            match pushed {
                Ok(_) => {
                    let closed = group.close(&mut area.writer);
                    group = area.commit(closed).clear();
                }
                Err(rejected) => break rejected,
            }
        };
        // Nine records of three blocks follow the restart block and leave four, of
        // which three stay free.
        let full = Full {
            needed: 3 * 4096,
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
        let rejected = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut vec![entry(header(1, Path::Live, 0), Parts::default())],
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
            area.push(&mut group, header(1, Path::Live, 2 * first), part.into());
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

    /// Pushes `batch` into an empty group and into a group with one entry, and
    /// checks that each refuses it with `limit` and stays as it was.
    fn assert_large(area: &mut Area, batch: &[Entry], limit: Limit) {
        let mut empty = Group::default();
        let pushed = push(
            &mut empty,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch.to_vec(),
        );
        assert_eq!(pushed, Err(Rejected::Large(limit)));
        assert!(empty.is_empty());
        assert!(empty.meta.is_none(), "a large batch takes no block");
        let mut group = Group::default();
        area.push(&mut group, header(9, Path::Live, 0), Parts::default());
        let pushed = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch.to_vec(),
        );
        assert_eq!(pushed, Err(Rejected::Large(limit)));
        assert_eq!(group.headers.len(), 1);
        assert_eq!(group.writes.len(), 0);
        assert_eq!(group.bytes, 0);
    }

    #[test]
    fn a_batch_alone_over_the_body_is_large() {
        let mut area = Area::new();
        let big = block(&area.pool, &vec![7; BODY_MAX - table_len(1) + 1]);
        let batch = [entry(header(1, Path::Live, 0), big.into())];
        let limit = Limit::Body {
            len: BODY_MAX + 1,
            max: BODY_MAX,
        };
        assert_large(&mut area, &batch, limit);
        assert_eq!(
            limit.to_string(),
            "the batch needs a record body of 8193 bytes, and a record of this ring \
             holds at most 8192"
        );
    }

    #[test]
    fn a_batch_alone_over_the_most_entries_is_large() {
        let mut area = Area::with_body_max(60_000);
        let batch =
            vec![entry(header(1, Path::Live, 0), Parts::default()); ENTRIES_MAX + 1];
        let limit = Limit::Entries { count: 1024 };
        assert_large(&mut area, &batch, limit);
        assert_eq!(
            limit.to_string(),
            "the batch has 1024 entries, and a record holds at most 1023"
        );
    }

    #[test]
    fn a_batch_alone_over_the_most_parts_is_large() {
        let mut area = Area::new();
        let two = parts(&area.pool, &[b"", b""]);
        let batch = vec![entry(header(1, Path::Live, 0), two); ENTRIES_MAX.div_ceil(2)];
        let limit = Limit::Parts { count: 1024 };
        assert_large(&mut area, &batch, limit);
        assert_eq!(
            limit.to_string(),
            "the batch has 1024 parts, and a record holds at most 1023"
        );
    }

    #[test]
    fn an_entry_over_block_max_is_large() {
        let mut area = Area::with_body_max(60_000);
        let half = vec![7; 3_000];
        let fits = parts(&area.pool, &[&half, &[7; 3_657]]);
        let over = parts(&area.pool, &[&half, &[7; 3_658]]);
        let big = vec![7; 31_000];
        let body = parts(&area.pool, &[&big, &big]);
        area.pool = pool(1 << 16);
        assert_eq!(area.block_max(), 6_657);
        let batch = [
            entry(header(1, Path::Live, 0), Parts::default()),
            entry(header(2, Path::Live, 0), over),
        ];
        let limit = Limit::Block {
            len: 6_658,
            max: 6_657,
        };
        assert_large(&mut area, &batch, limit);
        assert_eq!(
            limit.to_string(),
            "an entry has 6658 bytes of parts, and an entry of this buffer holds at \
             most 6657"
        );
        let both = [entry(header(1, Path::Live, 0), body)];
        let body = Limit::Body {
            len: table_len(1) + 62_000,
            max: 60_000,
        };
        assert_large(&mut area, &both, body);
        let mut group = Group::default();
        let mut batch = vec![entry(header(1, Path::Live, 0), fits)];
        let pushed = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch,
        );
        assert_eq!(pushed, Ok(0..1), "an entry at block_max fits");
    }

    #[test]
    fn block_and_body_come_before_a_tagged_entry() {
        let mut area = Area::with_body_max(60_000);
        let tagged = parts(&area.pool, &[&[7; 2_500], &[7; 2_500]]);
        let block = parts(&area.pool, &[&[7; 3_500], &[7; 3_500]]);
        let big = vec![7; 31_000];
        let body = parts(&area.pool, &[&big, &big]);
        area.pool = pool(1 << 16);
        assert_eq!(area.block_max(), 6_657);
        let tag = Header {
            tag: 3,
            ..header(1, Path::Live, 0)
        };
        let batch = [
            entry(tag, tagged.clone()),
            entry(header(2, Path::Live, 0), block),
        ];
        let limit = Limit::Block {
            len: 7_000,
            max: 6_657,
        };
        assert_large(&mut area, &batch, limit);
        let batch = [entry(tag, tagged), entry(header(2, Path::Live, 0), body)];
        let limit = Limit::Body {
            len: table_len(2) + 67_000,
            max: 60_000,
        };
        assert_large(&mut area, &batch, limit);
    }

    /// The first tagged entry over [`Layout::ENTRY_MAX_MIN`] names the limit, and the
    /// same bytes with tag 0 fit.
    #[test]
    fn refuses_a_tagged_entry_over_entry_max_min() {
        let mut area = Area::with_body_max(60_000);
        let over = parts(&area.pool, &[&[7; 2_000], &[7; 2_033]]);
        let at = parts(&area.pool, &[&[7; 4_032]]);
        let later = parts(&area.pool, &[&[7; 4_100]]);
        area.pool = pool(1 << 16);
        assert_eq!(area.block_max(), 6_657);
        let tagged = Header {
            tag: 3,
            ..header(1, Path::Live, 0)
        };
        let batch = [
            entry(header(2, Path::Live, 0), Parts::default()),
            entry(tagged, over.clone()),
            entry(tagged, later),
        ];
        let limit = Limit::Tagged {
            len: 4_033,
            max: 4_032,
        };
        assert_large(&mut area, &batch, limit);
        assert_eq!(
            limit.to_string(),
            "an entry with a tag has 4033 bytes of parts, and an entry with a tag \
             holds at most 4032"
        );
        let samples = Header { tag: 0, ..tagged };
        let mut group = Group::default();
        let mut batch = vec![entry(samples, over), entry(tagged, at)];
        let pushed = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch,
        );
        assert_eq!(pushed, Ok(0..2), "tag 0 and a tagged entry at the most fit");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// An empty group takes a batch under every limit, and refuses a batch over
        /// one with the first it is over: entries, then parts, then body. Each
        /// entry has `each` parts; the first part of the first entry has `len`
        /// bytes.
        #[test]
        fn an_empty_group_takes_any_batch_under_every_limit(
            count in prop_oneof![
                1..=3usize,
                ENTRIES_MAX / 2 - 2..=ENTRIES_MAX / 2 + 2,
                ENTRIES_MAX - 2..=ENTRIES_MAX + 4,
            ],
            each in 1..=2usize,
            len in 0..70_000usize,
        ) {
            let area = Area::with_body_max(60_000);
            let count_of_parts = count * each;
            let empty = parts(&area.pool, &vec![&b""[..]; each]);
            let mut batch = vec![entry(header(1, Path::Live, 0), empty); count];
            let bytes = vec![7; len];
            let mut first = vec![&b""[..]; each];
            first[0] = &bytes;
            batch[0] = entry(header(1, Path::Live, 0), parts(&area.pool, &first));
            let body = table_len(count) + len;
            let expected = if count > ENTRIES_MAX {
                Err(Rejected::Large(Limit::Entries { count }))
            } else if count_of_parts > ENTRIES_MAX {
                Err(Rejected::Large(Limit::Parts { count: count_of_parts }))
            } else if body > 60_000 {
                Err(Rejected::Large(Limit::Body { len: body, max: 60_000 }))
            } else {
                Ok(0..count)
            };
            let mut group = Group::default();
            let pushed = push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut batch,
            );
            prop_assert_eq!(pushed, expected.clone());
            prop_assert_eq!(group.is_empty(), expected.is_err());
            let checked = area.layout.check(count, count_of_parts, len);
            prop_assert_eq!(checked.map(|()| 0..count), expected.map_err(|rejected| {
                match rejected {
                    Rejected::Large(limit) => limit,
                    other => panic!("push gave {other:?}, not a limit"),
                }
            }));
        }

        /// One entry alone takes `entry_max` bytes of parts, and is large with one
        /// byte more.
        #[test]
        fn one_entry_alone_takes_the_entry_max(
            body_max in prop_oneof![Just(4087), 4087..=60_000usize],
        ) {
            let area = Area::with_body_max(body_max);
            let max = area.layout.entry_max();
            let push = |len: usize| {
                let bytes = vec![7; len];
                let parts = parts(&area.pool, &[&bytes]);
                let mut batch = vec![entry(header(1, Path::Live, 0), parts)];
                let block_max = area.block_max();
                push(
                    &mut Group::default(),
                    &area.pool,
                    block_max,
                    &area.writer,
                    &mut batch,
                )
            };
            prop_assert_eq!(push(max), Ok(0..1));
            let limit = Limit::Body { len: body_max + 1, max: body_max };
            prop_assert_eq!(push(max + 1), Err(Rejected::Large(limit)));
        }
    }

    #[test]
    fn a_batch_at_the_most_entries_or_parts_goes_in() {
        let area = Area::with_body_max(60_000);
        let mut batch =
            vec![entry(header(1, Path::Live, 0), Parts::default()); ENTRIES_MAX];
        let mut group = Group::default();
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut batch
            ),
            Ok(0..ENTRIES_MAX)
        );
        assert_eq!(group.headers.len(), ENTRIES_MAX);
        let two = parts(&area.pool, &[b"a", b"a"]);
        let mut batch = vec![entry(header(1, Path::Live, 0), two); ENTRIES_MAX / 2];
        batch.push(entry(header(1, Path::Live, 0), parts(&area.pool, &[b"a"])));
        let mut group = Group::default();
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut batch
            ),
            Ok(0..ENTRIES_MAX / 2 + 1)
        );
        assert_eq!(group.writes.len(), ENTRIES_MAX);
    }

    #[test]
    fn a_batch_over_two_limits_is_large_by_the_first() {
        let mut area = Area::new();
        let big = vec![7; BODY_MAX];
        let two = parts(&area.pool, &[b"", b""]);
        let mut over_parts =
            vec![entry(header(1, Path::Live, 0), two); ENTRIES_MAX.div_ceil(2)];
        over_parts[0] =
            entry(header(1, Path::Live, 0), parts(&area.pool, &[&big, b""]));
        let limit = Limit::Parts {
            count: ENTRIES_MAX + 1,
        };
        assert_large(&mut area, &over_parts, limit);
        let mut over_all = over_parts.clone();
        over_all.extend(over_parts.iter().cloned());
        let limit = Limit::Entries {
            count: ENTRIES_MAX + 1,
        };
        assert_large(&mut area, &over_all, limit);
    }

    #[test]
    fn a_batch_goes_in_whole_or_not_at_all() {
        let mut area = Area::new();
        let mut group = Group::default();
        let third = (BODY_MAX - table_len(3)) / 3;
        let part = block(&area.pool, &vec![7; third]);
        area.push(&mut group, header(1, Path::Live, 0), part.clone().into());
        let mut batch = vec![
            entry(header(1, Path::Live, 2), part.clone().into()),
            entry(header(2, Path::Live, 0), part.into()),
            entry(header(3, Path::Live, 0), Parts::default()),
        ];
        let rejected = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch,
        );
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        assert_eq!(group.bytes, third);
        assert_eq!(batch.len(), 3, "a refused batch keeps its entries");
        batch.remove(0);
        let pushed = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch,
        );
        assert_eq!(pushed, Ok(1..3));
        assert_eq!(group.headers.len(), 3);
        assert_eq!(group.slots, [slot(1), slot(2), slot(3)]);
        assert_eq!(group.bytes, 2 * third);
    }

    /// Records of nine times three blocks, two blocks, and one block follow the
    /// restart block. With the tail past the first record, a one-block record fits
    /// the last block and leaves the three blocks that each record keeps free, but
    /// the group with a second entry would skip it and not fit: the batch is not
    /// refused, the group closes first.
    #[test]
    fn a_batch_that_fits_its_own_record_asks_for_a_close_before_full() {
        let mut area = Area::new();
        let mut group = Group::default();
        let sizes = [BODY_MAX - table_len(1); 9].into_iter().chain([4096, 100]);
        for (first, size) in (0..).zip(sizes) {
            let part = block(&area.pool, &vec![7; size]);
            area.push(&mut group, header(1, Path::Live, 2 * first), part.into());
            let closed = group.close(&mut area.writer);
            let sealed = area.commit(closed);
            area.writer.synced(sealed.ends());
            if first == 0 {
                area.writer.release(sealed.ends().record.offset());
            }
            group = sealed.clear();
        }
        assert_eq!(area.writer.head(), 31 * 4096);
        let big = block(&area.pool, &[7; 4000]);
        area.push(&mut group, header(1, Path::Live, 20), big.into());
        let small = block(&area.pool, &[7; 100]);
        let mut batch = vec![entry(header(1, Path::Live, 22), small.into())];
        let rejected = push(
            &mut group,
            &area.pool,
            area.block_max(),
            &area.writer,
            &mut batch,
        );
        assert_eq!(rejected, Err(Rejected::Record));
        assert_eq!(group.headers.len(), 1);
        let closed = group.close(&mut area.writer);
        group = area.commit(closed).clear();
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut batch
            ),
            Ok(0..1)
        );
        let sealed = group.close(&mut area.writer).seal(area.chain);
        let places: Vec<u64> = sealed.writes().map(|(place, _)| place).collect();
        assert_eq!(places, [0], "the record wraps to the first block");
    }

    #[test]
    fn an_empty_batch_changes_nothing() {
        let area = Area::new();
        let mut group = Group::default();
        let lent = area.pool.committed();
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut Vec::new()
            ),
            Ok(0..0)
        );
        assert!(group.is_empty());
        assert!(group.meta.is_none(), "an empty batch takes no block");
        assert_eq!(area.pool.committed(), lent);
    }

    #[test]
    fn a_first_entry_the_pool_has_no_wrap_block_for_changes_nothing() {
        let mut area = Area::new();
        area.pool = pool(block::footprint(META_LEN) + 64);
        let mut group = Group::default();
        let rejected = push(
            &mut group,
            &area.pool,
            0,
            &area.writer,
            &mut vec![entry(header(1, Path::Live, 0), Parts::default())],
        );
        let exhausted = block::Error::Exhausted {
            requested: HEADER_LEN,
            available: 64,
        };
        assert_eq!(rejected, Err(Rejected::Pool(exhausted)));
        assert!(group.is_empty());
        assert!(group.meta.is_none(), "the meta block goes back");
        assert_eq!(area.pool.committed(), block::footprint(META_LEN));
    }

    #[test]
    fn parts_count_against_the_most_entries() {
        let area = Area::with_body_max(60_000);
        let mut group = Group::default();
        let two = parts(&area.pool, &[b"", b""]);
        let one = header(1, Path::Live, 0);
        let mut most = vec![entry(one, two); (ENTRIES_MAX - 1) / 2];
        let half = (ENTRIES_MAX - 1) / 2;
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut most
            ),
            Ok(0..half)
        );
        let mut rest =
            vec![entry(header(1, Path::Live, 2), parts(&area.pool, &[b""])); 2];
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut rest
            ),
            Err(Rejected::Record)
        );
        rest.pop();
        assert_eq!(
            push(
                &mut group,
                &area.pool,
                area.block_max(),
                &area.writer,
                &mut rest
            ),
            Ok(half..half + 1)
        );
        assert_eq!(group.writes.len(), ENTRIES_MAX);
    }
}
