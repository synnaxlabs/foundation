//! `Buffer::open` never panics on a ring file that a local writer changed, and what
//! `committed` reported durable is what a reopen gives.
//!
//! `Buffer::read` gives each path of a ring that opens as its doc says: every entry
//! up to the tail, a gap only for seqs that no entry holds, each entry that its
//! budget takes, and the same entries in one read, in reads of a small budget, from
//! inside an entry or a gap, and after a reopen.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! A batch makes a record of one to three blocks, with a table of up to three.
//! Two edits seal a CRC: a header block (at offset 42, over its first 512-byte
//! sector less the CRC) and a record (over `len`, `kind`, and the body, continued
//! from the chain the record before leaves: the header's chain field, a restart
//! record's body, or a record's CRC). One edit writes a tail and its chain value
//! in the first header block: a block of the area, on one of the first 128 laps or
//! the last 128. The records an open then writes wrap, or find the offsets at their
//! end.
//!
//! The edits restate the formats in `header.rs` and `record.rs` of `buffer`. Before
//! it edits, the target seals each record from the tail up to the first block with
//! no record, and writes the tail where it is. The build's ring is one chain from
//! one open, so when a seal or the write changes the file, or no record is found, a
//! format moved and the target panics.

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
/// Blocks in the area. A batch that needs more than are left gets `Rejected::Full`.
const BLOCKS: usize = 8;
const AREA: u64 = (BLOCKS * BLOCK) as u64;
/// A record's CRC, kind, and body: `[len: u32][crc32c: u32][kind: u8][body]`.
const RECORD_CRC_AT: usize = 4;
const RECORD_KIND_AT: usize = 8;
const RECORD_HEAD: usize = 9;
const DATA: u8 = 1;
const WRAP: u8 = 2;
const RESTART: u8 = 3;
/// The largest record is three blocks.
const BODY_MAX: usize = 3 * BLOCK - RECORD_HEAD;
/// The most bytes of one decoded entry. One such entry makes a record of three
/// blocks, and two make a batch that the layout refuses.
const PART_MAX: usize = 2 * BLOCK - 1;
/// The place of a header block's tail offset.
const HEADER_TAIL_AT: usize = 22;
/// The place of a header block's tail chain value.
const HEADER_CHAIN_AT: usize = 30;
/// The place of a header block's CRC, right after its fields.
const HEADER_CRC_AT: usize = 42;
/// A header block's CRC covers its first sector.
const SECTOR: usize = 512;
/// The two header blocks come before the area.
const FILE_LEN: usize = (2 + BLOCKS) * BLOCK;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const COMMIT: Span = Span::from_nanos(10_000_000);
const INDEXES: usize = 3;
/// The least and the most bytes of each entry the check commits: a record of one
/// block, and one of three.
const CHECK_PART: usize = 512;
const CHECK_PART_MAX: usize = 1994;
const _: () = assert!(check_record(CHECK_PART) <= BLOCK);
const _: () = assert!(check_record(CHECK_PART_MAX) <= 3 * BLOCK);
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];

/// The bytes of the check's record with entries of `part` bytes.
const fn check_record(part: usize) -> usize {
    let entries = INDEXES * PATHS.len();
    RECORD_HEAD + body(entries, entries * part)
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
                    block: u.int_in_range(2..=BLOCKS + 1)?,
                },
                _ => Edit::MoveTail {
                    block: u.int_in_range(2..=BLOCKS + 1)?,
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
        Edit::SealHeader { second } => {
            let start = if *second { BLOCK } else { 0 };
            let at = start + HEADER_CRC_AT;
            let crc = crc32c::crc32c(&image[start..at]);
            let crc = crc32c::crc32c_append(crc, &image[at + 4..start + SECTOR]);
            image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
        }
        Edit::SealRecord { block } => {
            let start = block * BLOCK;
            let chain = chain_before(image, *block);
            let record = &image[start..];
            let len = &record[..RECORD_CRC_AT];
            let claimed = u32::from_le_bytes(len.try_into().expect("four bytes"));
            let body = usize::try_from(claimed)
                .expect("invariant: a u32 fits in usize")
                .min(record.len() - RECORD_HEAD);
            let mut crc = crc32c::crc32c_append(chain, len);
            crc = crc32c::crc32c_append(crc, &record[RECORD_KIND_AT..RECORD_HEAD]);
            crc = crc32c::crc32c_append(crc, &record[RECORD_HEAD..RECORD_HEAD + body]);
            let at = start + RECORD_CRC_AT;
            image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
        }
        Edit::MoveTail { block, lap } => {
            let chain = chain_before(image, *block);
            let offset = lap * AREA + ((block - 2) * BLOCK) as u64;
            image[HEADER_TAIL_AT..HEADER_TAIL_AT + 8]
                .copy_from_slice(&offset.to_le_bytes());
            image[HEADER_CHAIN_AT..HEADER_CHAIN_AT + 4]
                .copy_from_slice(&chain.to_le_bytes());
            apply(image, &Edit::SealHeader { second: false });
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

/// The block and the lap of the tail in the first header block.
fn tail(image: &[u8]) -> (usize, u64) {
    let tail: [u8; 8] = image[HEADER_TAIL_AT..HEADER_TAIL_AT + 8]
        .try_into()
        .expect("invariant: eight bytes");
    let tail = u64::from_le_bytes(tail);
    (2 + (tail % AREA) as usize / BLOCK, tail / AREA)
}

/// The way of the records from the tail in the first header block, for `BLOCKS`
/// records: the block each starts at, with the chain value it continues from. The
/// CRCs on the way are not checked.
fn walk(image: &[u8]) -> Vec<(usize, u32)> {
    let at = |offset: usize| -> u32 {
        let bytes = image[offset..offset + 4]
            .try_into()
            .expect("invariant: four bytes");
        u32::from_le_bytes(bytes)
    };
    let (mut start, _) = tail(image);
    let mut chain = at(HEADER_CHAIN_AT);
    let mut way = vec![(start, chain)];
    for _ in 0..BLOCKS {
        let record = start * BLOCK;
        let kind = image[record + RECORD_KIND_AT];
        chain = at(record
            + if kind == RESTART {
                RECORD_HEAD
            } else {
                RECORD_CRC_AT
            });
        start = match kind {
            WRAP => 2,
            _ => start + (RECORD_HEAD + at(record) as usize).div_ceil(BLOCK),
        };
        if start == 2 + BLOCKS {
            start = 2;
        }
        way.push((start, chain));
        if start > 2 + BLOCKS {
            break;
        }
    }
    way
}

fn key(index: usize) -> channel::Key {
    channel::Key::from_u128(index as u128)
}

/// Opens the ring. Gives the slot of each index of the build, then each other slot
/// up to that of `SPARE`: an edit can change the index of an entry. An edit that
/// writes `SPARE` hides the slots after it.
async fn open(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
) -> std::result::Result<(Buffer, Vec<Slot>), Error> {
    let mut table = Slots::new();
    let config = Config {
        files: node.files(),
        dir: PathBuf::from(DIR),
        pool: Rc::clone(pool),
        clock: node.clock(),
        tasks: tasks.clone(),
        entropy: node.entropy(),
        layout: Layout::new(AREA, BODY_MAX).expect("invariant: the sizes make a ring"),
        commit: COMMIT,
    };
    let buffer = Buffer::open(config, &mut table).await?;
    let mut slots: Vec<Slot> =
        (0..INDEXES).map(|index| table.assign(key(index))).collect();
    let edited: Vec<Slot> = (0..=table.assign(SPARE).get())
        .map(Slot::new)
        .filter(|slot| !slots.contains(slot))
        .collect();
    slots.extend(edited);
    Ok((buffer, slots))
}

/// Writes the batches to a new ring and returns the durable tail and the entries of
/// each path, in slot then path order, and the blocks of the area its records take.
/// An append refuses a batch exactly when `limit` does, or when the blocks left are
/// fewer than the record takes, and the build goes on after it.
async fn build(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
) -> (Vec<Tail>, Vec<Vec<Given>>, usize) {
    let (buffer, slots) = open(node, tasks, pool).await.expect("a new ring opens");
    // A new ring holds its restart record, and no trim frees a block.
    let mut used = 1;
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
            let mut blocks = append.parts.iter().map(|len| {
                let mut block = pool.alloc(*len).expect("the pool has a block");
                block.fill(tag);
                block.freeze()
            });
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
        let size = (RECORD_HEAD + body(entries.len(), bytes)).div_ceil(BLOCK);
        let taken = buffer.append(entries);
        if let Err(limit) = fits {
            assert_eq!(
                taken,
                Err(Rejected::Large(limit)),
                "an append refused a batch other than by the record limits"
            );
            continue;
        }
        match taken {
            Ok(()) => assert!(
                used + size <= BLOCKS,
                "an append took more blocks than the area has"
            ),
            Err(Rejected::Full { .. }) => {
                assert!(
                    used + size > BLOCKS,
                    "an append refused a batch that the area has room for"
                );
                continue;
            }
            Err(other) => panic!("append failed: {other}"),
        }
        used += size;
        next = firsts;
        buffer.committed().await.expect("the batch commits");
        for (index, path, entry) in appended {
            written[index][path].push(entry);
        }
    }
    let written = written.into_iter().flatten().collect();
    (tails(&buffer, &slots), written, used)
}

/// The first limit of one record that a batch of `entries` entries, `parts` parts,
/// and `bytes` bytes is over: 1023 entries, 1023 parts, and a body of `body_max`.
fn limit(
    body_max: usize,
    entries: usize,
    parts: usize,
    bytes: usize,
) -> Result<(), Limit> {
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

/// Applies the edits to the ring file.
async fn edit(file: &File, pool: &Rc<Pool>, edits: &[Edit]) {
    let mut image = Vec::with_capacity(FILE_LEN);
    for block in 0..FILE_LEN / BLOCK {
        let into = pool.alloc(BLOCK).expect("the pool has a block");
        let read = file
            .read_at((block * BLOCK) as u64, into)
            .await
            .expect("the ring reads");
        image.extend_from_slice(&read);
    }
    let written = image.clone();
    let mut restated = 0;
    for (block, _) in walk(&image) {
        let kind = image.get(block * BLOCK + RECORD_KIND_AT);
        if !matches!(kind, Some(&(DATA | WRAP | RESTART))) {
            break;
        }
        apply(&mut image, &Edit::SealRecord { block });
        assert!(
            image == written,
            "the record seal restates the format at {block}"
        );
        restated += 1;
    }
    let (block, lap) = tail(&written);
    let mut image = written.clone();
    apply(&mut image, &Edit::MoveTail { block, lap });
    assert!(
        restated > 0 && image == written,
        "the record kind and `MoveTail` restate the formats"
    );
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
}

/// Opens the changed ring. With no edits, it must open, give the tails and the
/// entries the build left, and refuse the commit and the reopen for no room exactly
/// when the blocks of the area run out. When it opens, each path must read. One
/// commit on it must then give `Rejected::Large` when the record limits in the ring
/// header refuse it, or else fit or give `Rejected::Full`. A commit that fits must
/// survive a reopen and read back the same before and after.
///
/// The commit is one entry at each tail, of the input's `check_part` bytes. A record
/// edit can put a tail at `u64::MAX`, a precondition of `append`, so such a ring is
/// not checked.
async fn check(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
    built: &[Tail],
    written: &[Vec<Given>],
    used: usize,
) {
    // The blocks taken after the open when no edit changed the ring. An open writes
    // its restart record over the build's when the build took no batch. No input
    // fills the area without a batch, so the `wal` unit tests own that case.
    let used = input
        .edits
        .is_empty()
        .then_some(used + usize::from(used > 1));
    // The offsets left after a tail that the only edit moved to the last lap.
    let left = match input.edits[..] {
        [Edit::MoveTail { block, lap }] if lap == LAP_LAST => {
            Some(u64::MAX - (lap * AREA + ((block - 2) * BLOCK) as u64))
        }
        _ => None,
    };
    let opened = open(node, tasks, pool).await;
    if left.is_some_and(|left| left < BLOCK as u64) {
        assert!(
            matches!(opened, Err(Error::Full { .. } | Error::Invalid { .. })),
            "an open wrote a restart record past the last offset"
        );
    }
    let (buffer, slots) = match opened {
        Ok(opened) => opened,
        Err(Error::Pool(_) | Error::Files(_)) => panic!("open failed outside the ring"),
        Err(Error::Full { .. }) if used.is_none_or(|used| used > BLOCKS) => return,
        Err(other) if input.edits.is_empty() => {
            panic!("an open refused the ring the build wrote: {other}")
        }
        Err(_) => return,
    };
    assert!(
        used.is_none_or(|used| used <= BLOCKS),
        "an open wrote a restart record that the area has no room for"
    );
    let read = read_paths(&buffer, &slots).await;
    // An edit can change the sizes in the header, and the ring keeps them.
    let body_max = buffer.layout().body_max();
    if input.edits.is_empty() {
        assert_eq!(body_max, BODY_MAX, "an open lost the sizes the build wrote");
        assert_eq!(
            tails(&buffer, &slots),
            built,
            "an open lost what the build wrote"
        );
        let (build, rest) = read.split_at(written.len());
        assert_eq!(build, written, "a read lost what the build wrote");
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
            let mut part = pool.alloc(input.check_part).expect("the pool has a block");
            let tag = CHECK_TAG + u8::try_from(commits.len()).expect("few paths");
            part.fill(tag);
            let entry = Entry {
                index: key(index),
                slot: *slot,
                path,
                first: tail.seq,
                len: 1,
                stored_at: stored_at(tag),
                last: Some(CHECK_LAST),
                tag,
                parts: Parts::from(part.freeze()),
            };
            commits.push(Given::from(&entry));
            entries.push(entry);
        }
    }
    let before = tails(&buffer, &slots);
    // The restart record took a block of the offsets left.
    let room = left.map(|left| left.saturating_sub(BLOCK as u64));
    let fits = limit(
        body_max,
        entries.len(),
        entries.len(),
        input.check_part * entries.len(),
    );
    let taken = buffer.append(entries);
    if let Err(limit) = fits {
        assert_eq!(
            taken,
            Err(Rejected::Large(limit)),
            "an append refused a commit other than by the record limits"
        );
        return;
    }
    let used = used.map(|used| used + check_record(input.check_part).div_ceil(BLOCK));
    match taken {
        Ok(()) => assert!(
            room.is_none_or(|room| {
                check_record(input.check_part).next_multiple_of(BLOCK) as u64 <= room
            }),
            "a commit took offsets past the last"
        ),
        Err(Rejected::Full { free, .. }) => {
            assert!(
                room.is_none_or(|room| free <= room),
                "a full ring had more offsets free than are left"
            );
            assert!(
                used.is_none_or(|used| used > BLOCKS),
                "an append refused a commit that the area has room for"
            );
            return;
        }
        Err(other) => panic!("append failed: {other}"),
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
    assert!(
        used.is_none_or(|used| used <= BLOCKS),
        "an append took more blocks than the area has"
    );
    // An open costs one block for its restart record.
    let used = used.map(|used| used + 1);
    let (reopened, slots) = match open(node, tasks, pool).await {
        Ok(opened) => opened,
        Err(Error::Full { .. }) if used.is_none_or(|used| used > BLOCKS) => return,
        Err(other) => panic!("a ring with a commit did not reopen: {other}"),
    };
    assert!(
        used.is_none_or(|used| used <= BLOCKS),
        "a reopen wrote a restart record that the area has no room for"
    );
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
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config { budget: 1 << 21 };
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let (built, written, used) = build(&node, &tasks, &pool, &input).await;
            let file = node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            edit(&file, &pool, &input.edits).await;
            drop(file);
            check(&node, &tasks, &pool, &input, &built, &written, used).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
