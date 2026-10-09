//! `Buffer::open` never panics on a ring file that a local writer changed, and what
//! `committed` reported durable is what a reopen gives.
//!
//! `Buffer::read` gives each path of a ring that opens as its doc says: every entry
//! up to the tail, a gap only for seqs that no entry holds, each entry that its
//! budget takes, and the same entries in one read, in reads of a small budget, from
//! inside an entry or a gap, and after a reopen.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! A batch makes a record of one to four blocks, with a table of up to four.
//! Two edits seal a CRC: a header block and a record. One edit writes a tail and
//! its chain value in the first header block: a block of the area, on one of the
//! first 128 laps or the last 128.
//!
//! The target follows the offsets of the ring ([`Room`]) while no edit other than a
//! move of the tail changed it. Each append, open, and reopen must then fit, or give
//! `Full` with the bytes needed and free that the target expects. An open whose
//! records pass the last offset must give `Invalid`. The model has no trim (#160).
//! The limits of 1023 entries and 1023 parts are not reached, as a batch has at most
//! 259 entries: `checks_a_batch_against_each_limit_at_its_boundary` in `wal` and
//! `an_append_is_large_exactly_when_the_layout_check_fails` in the `buffer` tests own
//! them.
//!
//! The edits restate the formats in `header.rs` and `record.rs` of `buffer`. Before
//! it edits, the target walks the build's ring from the tail. Each record must have
//! a kind and seal as written, the records must end where the room of the build
//! ends, and a seal of each record and a write of the tail where it is must change
//! nothing. So when a format moves, the target panics. `WRAP` is restated and not
//! checked: the build never wraps.

#![no_main]

use std::iter;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{
    Buffer, Config, Entry, Error, Layout, Limit, Mark, Parts, Rejected, Stored, Tail,
};
use env::files::{File, Mode};
use env::tasks::Tasks;
use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use types::channel::{self, Slot, Slots};
use types::frame;
use types::time::{Span, Stamp};

const BLOCK: usize = 4096;
/// The largest record, in blocks.
const RECORD_BLOCKS: usize = 4;
/// Blocks in the area: four of the largest record, the least area of a ring. A power
/// of two, so the last lap of the offsets holds all of the area but its last byte.
const BLOCKS: usize = 4 * RECORD_BLOCKS;
const AREA: u64 = (BLOCKS * BLOCK) as u64;
/// The first block of the area: the two header blocks come before it.
const AREA_START: usize = 2;
const BODY_MAX: usize = RECORD_BLOCKS * BLOCK - record::HEAD;
/// The most bytes of one decoded entry. One such entry makes a record of three
/// blocks, and two make a batch over the body limit.
const PART_MAX: usize = 2 * BLOCK - 1;
const FILE_LEN: usize = (AREA_START + BLOCKS) * BLOCK;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const COMMIT: Span = Span::from_nanos(10_000_000);
const INDEXES: usize = 3;
/// The least and the most bytes of each entry the check commits: a record of one
/// block, and one of the largest.
const CHECK_PART: usize = 512;
const CHECK_PART_MAX: usize = 2677;
const _: () = assert!(check_record(CHECK_PART) <= BLOCK);
const _: () = assert!(check_record(CHECK_PART_MAX) <= RECORD_BLOCKS * BLOCK);
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];

/// The bytes of the check's record with entries of `part` bytes.
const fn check_record(part: usize) -> usize {
    let entries = INDEXES * PATHS.len();
    record::HEAD + body(entries, entries * part)
}

/// The body of a record of `entries` entries and `bytes` bytes: 4 bytes and 51 an
/// entry of table, then the bytes.
const fn body(entries: usize, bytes: usize) -> usize {
    4 + 51 * entries + bytes
}
/// A read budget that some entries of a path fill together.
const BUDGET: usize = 1024;
/// A key that no build writes. Its slot comes after each slot an open made.
const SPARE: channel::Key = channel::Key::from_u128(u128::MAX);
const CHECK_LAST: Stamp = Stamp::from_nanos(9);
/// The tag of the first entry of the build and of the check. Each entry takes the
/// next tag, fills its bytes with it, and is stored at it. The tags of the build
/// start again after 128 entries, below the tags of the check, so entries that
/// share a tag differ in `first`.
const BUILD_TAG: u8 = 0x01;
const CHECK_TAG: u8 = 0x81;
/// The last lap of the area that a `u64` offset holds.
const LAP_LAST: u64 = u64::MAX / AREA;
/// The last offset of a ring.
const OFFSET_LAST: u128 = u64::MAX as u128;

/// The ring offset of `block` of the file on `lap`.
fn offset(block: usize, lap: u64) -> u64 {
    lap * AREA + ((block - AREA_START) * BLOCK) as u64
}

/// The block of the file and the lap of ring offset `offset`: the inverse of
/// [`offset`] at a block boundary.
fn place(offset: u64) -> (usize, u64) {
    (AREA_START + (offset % AREA) as usize / BLOCK, offset / AREA)
}

fn word(image: &[u8], at: usize) -> u32 {
    let bytes = image[at..at + 4].try_into().expect("invariant: four bytes");
    u32::from_le_bytes(bytes)
}

/// The first header block of the file, as `header.rs` of `buffer` writes it.
mod header {
    use super::{BLOCK, word};

    const TAIL_AT: usize = 22;
    const CHAIN_AT: usize = 30;
    /// The CRC, right after the fields.
    const CRC_AT: usize = 42;
    /// The CRC covers the first sector of a header block.
    const SECTOR: usize = 512;

    /// The tail offset.
    pub(super) fn tail(image: &[u8]) -> u64 {
        let tail = image[TAIL_AT..TAIL_AT + 8]
            .try_into()
            .expect("invariant: eight bytes");
        u64::from_le_bytes(tail)
    }

    /// The chain value the record at the tail continues from.
    pub(super) fn chain(image: &[u8]) -> u32 {
        word(image, CHAIN_AT)
    }

    /// Writes the tail offset and its chain value, and seals the block.
    pub(super) fn put_tail(image: &mut [u8], tail: u64, chain: u32) {
        image[TAIL_AT..TAIL_AT + 8].copy_from_slice(&tail.to_le_bytes());
        image[CHAIN_AT..CHAIN_AT + 4].copy_from_slice(&chain.to_le_bytes());
        seal(image, false);
    }

    /// Seals the first header block, or the second.
    pub(super) fn seal(image: &mut [u8], second: bool) {
        let start = if second { BLOCK } else { 0 };
        let at = start + CRC_AT;
        let crc = crc32c::crc32c(&image[start..at]);
        let crc = crc32c::crc32c_append(crc, &image[at + 4..start + SECTOR]);
        image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
    }
}

/// A record of the area, as `record.rs` of `buffer` writes it:
/// `[len: u32][crc32c: u32][kind: u8][body]`.
mod record {
    use super::{BLOCK, word};

    const CRC_AT: usize = 4;
    pub(super) const KIND_AT: usize = 8;
    pub(super) const HEAD: usize = 9;
    pub(super) const DATA: u8 = 1;
    pub(super) const WRAP: u8 = 2;
    pub(super) const RESTART: u8 = 3;

    pub(super) fn len(image: &[u8], block: usize) -> usize {
        usize::try_from(word(image, block * BLOCK)).expect("invariant: a u32 fits")
    }

    pub(super) fn kind(image: &[u8], block: usize) -> u8 {
        image[block * BLOCK + KIND_AT]
    }

    /// The chain value the record leaves: a restart record's body, or its CRC.
    pub(super) fn chain(image: &[u8], block: usize) -> u32 {
        let at = if kind(image, block) == RESTART {
            HEAD
        } else {
            CRC_AT
        };
        word(image, block * BLOCK + at)
    }

    /// The CRC of the record at `block` when it continues from `chain`: over `len`,
    /// `kind`, and the body, cut at the end of the file.
    fn crc(image: &[u8], block: usize, chain: u32) -> u32 {
        let record = &image[block * BLOCK..];
        let body = len(image, block).min(record.len() - HEAD);
        let crc = crc32c::crc32c_append(chain, &record[..CRC_AT]);
        crc32c::crc32c_append(crc, &record[KIND_AT..HEAD + body])
    }

    /// Writes the CRC of the record at `block` when it continues from `chain`.
    pub(super) fn seal(image: &mut [u8], block: usize, chain: u32) {
        let crc = crc(image, block, chain);
        let at = block * BLOCK + CRC_AT;
        image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Whether the record at `block` has a kind and its CRC continues from `chain`.
    pub(super) fn sealed(image: &[u8], block: usize, chain: u32) -> bool {
        matches!(kind(image, block), DATA | WRAP | RESTART)
            && crc(image, block, chain) == word(image, block * BLOCK + CRC_AT)
    }
}

/// What a write of the ring must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    Fits,
    /// It must give `Full` with these bytes.
    Full {
        needed: u64,
        free: u64,
    },
    /// An open must give `Invalid`: a record it walks passes the last offset.
    Past,
    /// The target does not follow the ring after the edits.
    Unknown,
}

impl Fit {
    /// Panics when a refusal for no room is not what the fit says.
    fn refused(self, needed: u64, free: u64) {
        assert!(
            matches!(self, Self::Unknown) || self == Self::Full { needed, free },
            "the ring had no room for {needed} bytes with {free} free, against {self:?}"
        );
    }
}

/// The offsets of a ring that the target follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Offsets {
    tail: u128,
    /// The end of the last data record: where an open puts its restart record.
    data: u128,
    /// The end of the last record: where the next record goes.
    end: u128,
}

impl Offsets {
    /// The bytes that a record of `size` bytes at `at` takes with the rest of the
    /// area it skips, as `Writer::cost` of `buffer` gives them.
    fn cost(self, at: u128, size: u128) -> std::result::Result<u128, Fit> {
        let area = u128::from(AREA);
        let rest = area - at % area;
        let skipped = if size > rest { rest } else { 0 };
        let free = (area - (at - self.tail)).min(OFFSET_LAST - at);
        let needed = skipped + size;
        let bytes = |bytes| u64::try_from(bytes).expect("invariant: under an area");
        if needed > free {
            return Err(Fit::Full {
                needed: bytes(needed),
                free: bytes(free),
            });
        }
        Ok(needed)
    }
}

/// The room of the ring as the target follows it. It has no trim, so no write
/// frees room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Room(Option<Offsets>);

impl Room {
    /// A new ring: its restart record at offset 0.
    fn new() -> Self {
        Self(Some(Offsets {
            tail: 0,
            data: 0,
            end: BLOCK as u128,
        }))
    }

    /// The room an open finds in `image` after `edits`, from the records on the
    /// way from the tail that are sealed. Unknown after an edit other than
    /// `MoveTail`, whose records the open can read in other ways.
    fn walked(image: &[u8], edits: &[Edit]) -> Self {
        if !edits
            .iter()
            .all(|edit| matches!(edit, Edit::MoveTail { .. }))
        {
            return Self(None);
        }
        let way = walk(image);
        let tail = u128::from(header::tail(image));
        let (mut data, mut end) = (tail, tail);
        for step in way.windows(2) {
            let [(block, chain), (next, _)] = *step else {
                unreachable!("a window of two")
            };
            if next > AREA_START + BLOCKS || !record::sealed(image, block, chain) {
                break;
            }
            let blocks = if next > block {
                next - block
            } else {
                next + BLOCKS - block
            };
            end += (blocks * BLOCK) as u128;
            if record::kind(image, block) == record::DATA {
                data = end;
            }
        }
        Self(Some(Offsets { tail, data, end }))
    }

    /// An open: it walks to the end of the last record, then puts its restart
    /// record of one block at the end of the last data record.
    fn open(&mut self) -> Fit {
        let Some(offsets) = &mut self.0 else {
            return Fit::Unknown;
        };
        if offsets.end > OFFSET_LAST {
            return Fit::Past;
        }
        match offsets.cost(offsets.data, BLOCK as u128) {
            Ok(needed) => {
                offsets.end = offsets.data + needed;
                Fit::Fits
            }
            Err(full) => full,
        }
    }

    /// An append of a record of `blocks` blocks after the last record.
    fn take(&mut self, blocks: usize) -> Fit {
        let Some(offsets) = &mut self.0 else {
            return Fit::Unknown;
        };
        match offsets.cost(offsets.end, (blocks * BLOCK) as u128) {
            Ok(needed) => {
                offsets.end += needed;
                offsets.data = offsets.end;
                Fit::Fits
            }
            Err(full) => full,
        }
    }
}

/// Whether an append took its batch. Panics when that is not what `fit` says.
fn took(taken: std::result::Result<(), Rejected>, fit: Fit) -> bool {
    match (taken, fit) {
        (Ok(()), Fit::Fits | Fit::Unknown) => true,
        (Err(Rejected::Full { needed, free }), _) => {
            fit.refused(needed, free);
            false
        }
        (taken, fit) => panic!("an append gave {taken:?} where the ring gives {fit:?}"),
    }
}

/// The time an entry with `tag` is stored.
fn stored_at(tag: u8) -> Stamp {
    Stamp::from_nanos(i64::from(tag))
}

/// One entry of a path, as appended or as a read gave it, with its bytes off the
/// pool, so the entries of one read do not take the blocks of the next.
#[derive(Debug, PartialEq, Eq)]
struct Given {
    first: u64,
    len: u32,
    stored_at: Stamp,
    last: Option<Stamp>,
    tag: u8,
    bytes: Vec<u8>,
}

impl From<&Stored> for Given {
    fn from(entry: &Stored) -> Self {
        let Stored {
            first,
            len,
            stored_at,
            last,
            tag,
            bytes,
        } = entry;
        Self {
            first: *first,
            len: *len,
            stored_at: *stored_at,
            last: *last,
            tag: *tag,
            bytes: bytes.to_vec(),
        }
    }
}

impl From<&Entry> for Given {
    fn from(entry: &Entry) -> Self {
        Self {
            first: entry.first,
            len: entry.len,
            stored_at: entry.stored_at,
            last: entry.last,
            tag: entry.tag,
            bytes: entry
                .parts
                .clone()
                .into_iter()
                .flat_map(|b| b.to_vec())
                .collect(),
        }
    }
}

/// One entry the build phase appends.
#[derive(Debug)]
struct Append {
    index: usize,
    path: frame::Path,
    /// Seqs the entry leaves out before its first.
    skip: u64,
    /// Samples in the entry: 0 for a caller record.
    len: u32,
    /// The bytes of each part: none, one, or two.
    parts: Vec<usize>,
}

impl Append {
    /// Small entry `number` of a batch, with at most 72 bytes. The entries differ:
    /// samples or a caller record, no part or two, a skip ahead or none.
    fn small(number: usize) -> Self {
        Self {
            index: number % INDEXES,
            path: PATHS[number / INDEXES % PATHS.len()],
            skip: if number % 5 == 2 { 2 } else { 0 },
            len: u32::from(number % 4 != 3),
            parts: if number % 7 == 1 {
                vec![number % 61, number % 13]
            } else {
                Vec::new()
            },
        }
    }
}

/// One change to the file bytes.
#[derive(Debug)]
enum Edit {
    Put { at: usize, bytes: Vec<u8> },
    Zero { at: usize, len: usize },
    SealHeader { second: bool },
    SealRecord { block: usize },
    MoveTail { block: usize, lap: u64 },
}

#[derive(Debug)]
struct Input {
    /// The replay value of the run, from the input bytes. Bytes on a disk cannot
    /// hold a chain that a later open draws, so an edit must not either.
    replay: u64,
    batches: Vec<Vec<Append>>,
    edits: Vec<Edit>,
    /// Bytes of each entry the check commits.
    check_part: usize,
}

/// FNV-1a of 64 bits. Its definition is fixed, so a stored input keeps its run.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let replay = fnv(u.peek_bytes(u.len()).expect("the input has its length"));
        let mut batches = Vec::new();
        for _ in 0..u.int_in_range(0..=4)? {
            let mut appends = Vec::new();
            for _ in 0..u.int_in_range(1..=4)? {
                appends.push(Append {
                    index: u.int_in_range(0..=INDEXES - 1)?,
                    path: PATHS[usize::from(u.int_in_range(0..=1u8)?)],
                    skip: 0,
                    len: u.int_in_range(1..=16)?,
                    parts: vec![u.int_in_range(0..=PART_MAX)?],
                });
            }
            batches.push(appends);
        }
        let blocks = AREA_START..=AREA_START + BLOCKS - 1;
        let mut edits = Vec::new();
        for _ in 0..u.int_in_range(0..=16)? {
            edits.push(match u.int_in_range(0..=4u8)? {
                0 => Edit::Put {
                    at: u.int_in_range(0..=FILE_LEN - 1)?,
                    bytes: u.arbitrary::<[u8; 8]>()?[..u.int_in_range(1..=8)?].to_vec(),
                },
                1 => Edit::Zero {
                    at: u.int_in_range(0..=FILE_LEN - 1)?,
                    len: u.int_in_range(1..=FILE_LEN)?,
                },
                2 => Edit::SealHeader {
                    second: u.arbitrary()?,
                },
                3 => Edit::SealRecord {
                    block: u.int_in_range(blocks.clone())?,
                },
                _ => Edit::MoveTail {
                    block: u.int_in_range(blocks.clone())?,
                    // The first 128 laps, or the last 128.
                    lap: match u.arbitrary::<u8>()? {
                        low @ ..128 => u64::from(low),
                        high => LAP_LAST - u64::from(255 - high),
                    },
                },
            });
        }
        // Last, so an input that ends before these has no small entry and the least
        // commit.
        for batch in &mut batches {
            batch.extend((0..u.int_in_range(0..=255)?).map(Append::small));
        }
        Ok(Self {
            replay,
            batches,
            edits,
            check_part: u.int_in_range(CHECK_PART..=CHECK_PART_MAX)?,
        })
    }
}

fn apply(image: &mut [u8], edit: &Edit) {
    match edit {
        Edit::Put { at, bytes } => {
            let len = bytes.len().min(image.len() - at);
            image[*at..at + len].copy_from_slice(&bytes[..len]);
        }
        Edit::Zero { at, len } => {
            let len = (*len).min(image.len() - at);
            image[*at..at + len].fill(0);
        }
        Edit::SealHeader { second } => header::seal(image, *second),
        Edit::SealRecord { block } => {
            let chain = chain_before(image, *block);
            record::seal(image, *block, chain);
        }
        Edit::MoveTail { block, lap } => {
            let chain = chain_before(image, *block);
            header::put_tail(image, offset(*block, *lap), chain);
        }
    }
}

/// The chain value a record at `block` must continue from: the one that the
/// records from the tail in the first header block leave there. When no record
/// starts at `block`, the value where the way ends.
fn chain_before(image: &[u8], block: usize) -> u32 {
    let way = walk(image);
    let end = way.last().expect("invariant: the way starts at the tail");
    way.iter()
        .find(|&&(start, _)| start == block)
        .unwrap_or(end)
        .1
}

/// The way of the records from the tail in the first header block: the block each
/// starts at with the chain value it continues from, then the block and the chain
/// value after the last. It ends where it comes back to the tail or passes the end
/// of the area. The CRCs on the way are not checked.
fn walk(image: &[u8]) -> Vec<(usize, u32)> {
    let (tail, _) = place(header::tail(image));
    let mut way = vec![(tail, header::chain(image))];
    let mut block = tail;
    for _ in 0..BLOCKS {
        let next = match record::kind(image, block) {
            record::WRAP => AREA_START,
            _ => block + (record::HEAD + record::len(image, block)).div_ceil(BLOCK),
        };
        let next = if next == AREA_START + BLOCKS {
            AREA_START
        } else {
            next
        };
        way.push((next, record::chain(image, block)));
        if next == tail || next > AREA_START + BLOCKS {
            break;
        }
        block = next;
    }
    way
}

fn key(index: usize) -> channel::Key {
    channel::Key::from_u128(index as u128)
}

/// The shard of the run.
struct Shard {
    node: sim::node::Node,
    tasks: Tasks,
    pool: Rc<Pool>,
}

impl Shard {
    /// Opens the ring. Gives the slot of each index of the build, then each other
    /// slot up to that of `SPARE`: an edit can change the index of an entry. An edit
    /// that writes `SPARE` hides the slots after it.
    async fn open(&self) -> std::result::Result<(Buffer, Vec<Slot>), Error> {
        let mut table = Slots::new();
        let config = Config {
            files: self.node.files(),
            dir: PathBuf::from(DIR),
            pool: Rc::clone(&self.pool),
            clock: self.node.clock(),
            tasks: self.tasks.clone(),
            entropy: self.node.entropy(),
            layout: Layout::new(AREA, BODY_MAX)
                .expect("invariant: the sizes make a ring"),
            commit: COMMIT,
        };
        let buffer = Buffer::open(config, &mut table).await?;
        let mut slots: Vec<Slot> =
            (0..INDEXES).map(|index| table.index(key(index))).collect();
        let edited: Vec<Slot> = (0..=table.data(SPARE).get())
            .map(Slot::new)
            .filter(|slot| !slots.contains(slot))
            .collect();
        slots.extend(edited);
        Ok((buffer, slots))
    }

    /// A block of `len` bytes of `tag`.
    fn block(&self, len: usize, tag: u8) -> Block {
        let mut block = self.pool.alloc(len).expect("the pool has a block");
        block.fill(tag);
        block.freeze()
    }
}

/// What the build left.
struct Built {
    /// The durable tail of each path, in slot then path order.
    tails: Vec<Tail>,
    /// The entries of each path, in the same order.
    written: Vec<Vec<Given>>,
    room: Room,
}

/// Writes the batches to a new ring. An append refuses a batch exactly when `limit`
/// does, or else as the room says, and the build goes on after it.
async fn build(shard: &Shard, input: &Input) -> Built {
    let (buffer, slots) = shard.open().await.expect("a new ring opens");
    let mut room = Room::new();
    let mut next = vec![[0u64; 2]; INDEXES];
    let mut tags = (BUILD_TAG..CHECK_TAG).cycle();
    let mut written: Vec<[Vec<Given>; 2]> =
        iter::repeat_with(Default::default).take(INDEXES).collect();
    for batch in &input.batches {
        let mut firsts = next.clone();
        let mut appended = Vec::new();
        let mut entries = Vec::new();
        let (mut parts, mut bytes) = (0, 0);
        for (append, tag) in batch.iter().zip(&mut tags) {
            let path = usize::from(append.path == frame::Path::Backfill);
            let first = firsts[append.index][path] + append.skip;
            firsts[append.index][path] = first + u64::from(append.len);
            let last = (append.len > 0)
                .then(|| Stamp::from_nanos(i64::try_from(first).expect("small")));
            let mut blocks = append.parts.iter().map(|len| shard.block(*len, tag));
            let blocks = match (blocks.next(), blocks.next()) {
                (Some(one), Some(two)) => Parts::from([one, two]),
                (one, _) => Parts::from(one),
            };
            parts += append.parts.len();
            let entry = Entry {
                index: key(append.index),
                slot: slots[append.index],
                path: append.path,
                first,
                len: append.len,
                stored_at: stored_at(tag),
                last,
                tag,
                parts: blocks,
            };
            let given = Given::from(&entry);
            bytes += given.bytes.len();
            appended.push((append.index, path, given));
            entries.push(entry);
        }
        let fits = limit(BODY_MAX, entries.len(), parts, bytes);
        let checked = buffer.layout().check(entries.len(), parts, bytes);
        assert_eq!(checked, fits, "the layout check is not the record limits");
        let blocks = (record::HEAD + body(entries.len(), bytes)).div_ceil(BLOCK);
        let taken = buffer.append(entries);
        if let Err(limit) = fits {
            assert_eq!(
                taken,
                Err(Rejected::Large(limit)),
                "an append refused a batch other than by the record limits"
            );
            continue;
        }
        if !took(taken, room.take(blocks)) {
            continue;
        }
        next = firsts;
        buffer.committed().await.expect("the batch commits");
        for (index, path, entry) in appended {
            written[index][path].push(entry);
        }
    }
    Built {
        tails: tails(&buffer, &slots),
        written: written.into_iter().flatten().collect(),
        room,
    }
}

/// The first limit of one record that a batch of `entries` entries, `parts` parts,
/// and `bytes` bytes is over: 1023 entries, 1023 parts, and a body of `body_max`.
fn limit(
    body_max: usize,
    entries: usize,
    parts: usize,
    bytes: usize,
) -> std::result::Result<(), Limit> {
    const COUNT_MAX: usize = 1023;
    let len = body(entries, bytes);
    if entries > COUNT_MAX {
        Err(Limit::Entries { count: entries })
    } else if parts > COUNT_MAX {
        Err(Limit::Parts { count: parts })
    } else if len > body_max {
        Err(Limit::Body { len, max: body_max })
    } else {
        Ok(())
    }
}

/// Every entry of each path from the start, in slot then path order.
async fn read_paths(buffer: &Buffer, slots: &[Slot]) -> Vec<Vec<Given>> {
    let mut paths = Vec::new();
    for slot in slots {
        for path in PATHS {
            let whole = read_path(buffer, *slot, path, usize::MAX).await;
            for budget in [1, BUDGET] {
                let stepped = read_path(buffer, *slot, path, budget).await;
                assert_eq!(stepped, whole, "reads of {budget} gave other entries");
            }
            let mut at = 0;
            for entry in &whole {
                if entry.first - at > 1 {
                    read_from(buffer, *slot, path, at + 1, entry).await;
                }
                if entry.len > 1 {
                    read_from(buffer, *slot, path, entry.first + 1, entry).await;
                }
                at = entry.first + u64::from(entry.len);
            }
            paths.push(whole);
        }
    }
    paths
}

/// Reads from seq `from`, which is in the gap before `entry` or inside it. The read
/// must report the rest of the gap and give `entry` first, whole.
async fn read_from(
    buffer: &Buffer,
    slot: Slot,
    path: frame::Path,
    from: u64,
    entry: &Given,
) {
    let read = buffer
        .read(slot, path, Mark::at(from), 1)
        .await
        .expect("a read of an open ring");
    let gap = (entry.first > from).then_some(from..entry.first);
    assert_eq!(read.gap, gap, "the gap of a read from seq {from}");
    let first = read.entries.first().map(Given::from);
    assert_eq!(first.as_ref(), Some(entry), "a read from seq {from}");
}

/// Every entry of `path` from the start, in reads of `budget`.
///
/// # Panics
///
/// When a read breaks the doc of `Buffer::read`.
async fn read_path(
    buffer: &Buffer,
    slot: Slot,
    path: frame::Path,
    budget: usize,
) -> Vec<Given> {
    let tail = buffer.durable(slot, path);
    let mut given: Vec<Given> = Vec::new();
    let mut from = Mark::at(0);
    let mut under = false;
    loop {
        let read = buffer
            .read(slot, path, from, budget)
            .await
            .expect("a read of an open ring");
        assert!(read.next.seq <= tail.seq, "a read went past the tail");
        if read.entries.is_empty() {
            assert_eq!(read.gap, None, "a gap with no entry");
            assert_eq!(read.next, from, "an empty read moved the mark");
            assert_eq!(from.seq, tail.seq, "the reads did not end at the tail");
            let stamp = given.iter().rev().find_map(|entry| entry.last);
            assert_eq!(stamp, tail.stamp, "the reads did not give the tail stamp");
            return given;
        }
        assert!(
            !under || read.gap.is_some(),
            "a read stopped before its budget, a skip, or the tail"
        );
        let mut at = from.seq;
        let mut spent = 0;
        for (number, entry) in read.entries.iter().enumerate() {
            assert!(spent < budget, "entry {number} came past the budget");
            spent += block::footprint(entry.bytes.len());
            if number == 0 {
                assert!(entry.first >= at, "the first entry is below the mark");
                let gap = (entry.first > at).then_some(at..entry.first);
                assert_eq!(read.gap, gap, "the gap before the first entry");
            } else {
                assert_eq!(entry.first, at, "a gap before entry {number}");
            }
            at = entry
                .first
                .checked_add(u64::from(entry.len))
                .expect("an entry ends past the last seq");
            given.push(Given::from(entry));
        }
        assert_eq!(read.next.seq, at, "next is not after the last entry");
        assert!(
            read.next > from,
            "a read with entries did not move the mark"
        );
        under = spent < budget;
        from = read.next;
    }
}

/// The durable tail of each path, in slot then path order.
fn tails(buffer: &Buffer, slots: &[Slot]) -> Vec<Tail> {
    slots
        .iter()
        .flat_map(|slot| PATHS.map(|path| buffer.durable(*slot, path)))
        .collect()
}

/// Applies the edits to the ring file, and gives the room an open then finds.
async fn edit(file: &File, pool: &Rc<Pool>, edits: &[Edit], built: Room) -> Room {
    let mut image = Vec::with_capacity(FILE_LEN);
    for block in 0..FILE_LEN / BLOCK {
        let into = pool.alloc(BLOCK).expect("the pool has a block");
        let read = file
            .read_at((block * BLOCK) as u64, into)
            .await
            .expect("the ring reads");
        image.extend_from_slice(&read);
    }
    assert_eq!(
        Room::walked(&image, &[]),
        built,
        "the records of the build do not seal as written up to its last"
    );
    let written = image.clone();
    let way = walk(&written);
    let sealed =
        |&&(block, chain): &&(usize, u32)| record::sealed(&written, block, chain);
    for &(block, _) in way.iter().take_while(sealed) {
        apply(&mut image, &Edit::SealRecord { block });
        assert!(
            image == written,
            "`SealRecord` restates the record at {block}"
        );
    }
    let (block, lap) = place(header::tail(&image));
    apply(&mut image, &Edit::MoveTail { block, lap });
    assert!(image == written, "`MoveTail` restates the header");
    for edit in edits {
        apply(&mut image, edit);
    }
    let blocks: Vec<Block> = image
        .chunks(BLOCK)
        .map(|bytes| {
            let mut block = pool.alloc(BLOCK).expect("the pool has a block");
            block.copy_from_slice(bytes);
            block.freeze()
        })
        .collect();
    file.write_at(0, &blocks).await.expect("the ring writes");
    file.sync().await.expect("the ring syncs");
    Room::walked(&image, edits)
}

/// Opens the changed ring, which must open or refuse as `room` says. With no edits,
/// it must give the tails, the entries, and the sizes the build left. When it opens,
/// each path must read. One commit on it must then give `Rejected::Large` when the
/// record limits in the ring header refuse it, or else fit or give `Rejected::Full`
/// as `room` says. A commit that fits must survive a reopen and read back the same
/// before and after.
///
/// The commit is one entry at each tail, of the input's `check_part` bytes. A record
/// edit can put a tail at `u64::MAX`, a precondition of `append`, so such a ring is
/// not checked.
async fn check(shard: &Shard, input: &Input, built: &Built, mut room: Room) {
    let fit = room.open();
    let (buffer, slots) = match (shard.open().await, fit) {
        (Err(Error::Pool(_) | Error::Files(_)), _) => {
            panic!("open failed outside the ring")
        }
        (Ok(opened), Fit::Fits | Fit::Unknown) => opened,
        (Err(Error::Full { needed, free }), _) => return fit.refused(needed, free),
        (Err(Error::Invalid { .. }), Fit::Past) | (Err(_), Fit::Unknown) => return,
        (opened, fit) => {
            let opened = opened.map(drop);
            panic!("an open gave {opened:?} where the ring gives {fit:?}")
        }
    };
    let read = read_paths(&buffer, &slots).await;
    // An edit can change the sizes in the header, and the ring keeps them.
    let body_max = buffer.layout().body_max();
    if input.edits.is_empty() {
        assert_eq!(body_max, BODY_MAX, "an open lost the sizes the build wrote");
        assert_eq!(
            tails(&buffer, &slots),
            built.tails,
            "an open lost what the build wrote"
        );
        let (build, rest) = read.split_at(built.written.len());
        assert_eq!(build, built.written, "a read lost what the build wrote");
        assert!(
            rest.iter().all(Vec::is_empty),
            "a read gave what no build wrote"
        );
    }
    let mut entries = Vec::new();
    let mut commits = Vec::new();
    for (index, slot) in slots[..INDEXES].iter().enumerate() {
        for path in PATHS {
            let tail = buffer.tail(*slot, path);
            if tail.seq == u64::MAX {
                return;
            }
            let tag = CHECK_TAG + u8::try_from(commits.len()).expect("few paths");
            let entry = Entry {
                index: key(index),
                slot: *slot,
                path,
                first: tail.seq,
                len: 1,
                stored_at: stored_at(tag),
                last: Some(CHECK_LAST),
                tag,
                parts: Parts::from(shard.block(input.check_part, tag)),
            };
            commits.push(Given::from(&entry));
            entries.push(entry);
        }
    }
    let before = tails(&buffer, &slots);
    let count = entries.len();
    let bytes = input.check_part * count;
    let fits = limit(body_max, count, count, bytes);
    let checked = buffer.layout().check(count, count, bytes);
    assert_eq!(checked, fits, "the layout check is not the record limits");
    let taken = buffer.append(entries);
    if let Err(limit) = fits {
        assert_eq!(
            taken,
            Err(Rejected::Large(limit)),
            "an append refused a commit other than by the record limits"
        );
        return;
    }
    let blocks = check_record(input.check_part).div_ceil(BLOCK);
    if !took(taken, room.take(blocks)) {
        return;
    }
    assert_eq!(
        tails(&buffer, &slots),
        before,
        "an entry was durable before its commit"
    );
    buffer.committed().await.expect("the entries commit");
    let durable = tails(&buffer, &slots);
    let stored = read_paths(&buffer, &slots).await;
    // The paths of the build come first.
    for (given, commit) in stored.iter().zip(&commits) {
        assert_eq!(given.last(), Some(commit), "a read lost the commit");
    }
    drop(buffer);
    let fit = room.open();
    let (reopened, slots) = match (shard.open().await, fit) {
        (Ok(opened), Fit::Fits | Fit::Unknown) => opened,
        (Err(Error::Full { needed, free }), _) => return fit.refused(needed, free),
        (opened, fit) => {
            let opened = opened.map(drop);
            panic!(
                "a reopen after a commit gave {opened:?} where the ring gives {fit:?}"
            )
        }
    };
    let recovered: Vec<Tail> = slots
        .iter()
        .flat_map(|slot| PATHS.map(|path| reopened.tail(*slot, path)))
        .collect();
    assert_eq!(recovered, durable, "a reopen lost what committed reported");
    assert_eq!(
        read_paths(&reopened, &slots).await,
        stored,
        "a reopen changed what a read gives"
    );
}

fuzz_target!(|input: Input| {
    let mut sim = sim::Sim::new(sim::Config {
        seed: input.replay,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let config = env::shards::Config {
        name: DIR.into(),
        core: None,
    };
    let handle = node
        .clone()
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config { budget: 1 << 21 };
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let shard = Shard { node, tasks, pool };
            let built = build(&shard, &input).await;
            let file = shard
                .node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            let room = edit(&file, &shard.pool, &input.edits, built.room).await;
            drop(file);
            check(&shard, &input, &built, room).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
