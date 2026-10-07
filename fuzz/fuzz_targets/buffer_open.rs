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
//! record's body, or a record's CRC). One edit moves the tail in the first header
//! block to a block of the area, on any lap, as a trim does: the records an open
//! then writes wrap, or find the offsets at their end. A wide input commits a
//! record of two blocks in the check, so a wrap record can come before it.
//! They restate the formats in `header.rs` and `record.rs` of `buffer`. When the
//! header moves, the seal of the first block as written changes it and the target
//! panics. When the record moves, the edits stop reaching the walk and coverage
//! drops without a failed replay.

#![no_main]

use std::iter;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{
    Buffer, Config, Entry, Error, Layout, Mark, Parts, Rejected, Stored, Tail,
};
use env::files::{File, Mode};
use env::tasks::Tasks;
use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use types::channel::{self, Slot, Slots};
use types::frame;
use types::time::{Span, Stamp};

const BLOCK: usize = 4096;
/// Blocks in the area: room for the build, the check, and the restart record each
/// open writes.
const BLOCKS: usize = 8;
const AREA: u64 = (BLOCKS * BLOCK) as u64;
/// A record is 9 bytes of header and a body, so the largest record is three blocks.
const BODY_MAX: usize = 3 * BLOCK - 9;
/// The most bytes of one appended entry: two blocks less one byte.
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
/// Bytes of each entry the check commits: a record of one block, or of two.
const CHECK_PART: usize = 512;
const CHECK_PART_WIDE: usize = 1024;
/// The check's batch and its table, of 51 bytes an entry, fit one record.
const _: () = assert!(4 + INDEXES * PATHS.len() * (51 + CHECK_PART_WIDE) <= BODY_MAX);
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];
/// A read budget that some entries of a path fill together.
const BUDGET: usize = 1024;
/// A key that no build writes. Its slot comes after each slot an open made.
const SPARE: channel::Key = channel::Key::from_u128(u128::MAX);
const STORED_AT: Stamp = Stamp::from_nanos(7);
const CHECK_LAST: Stamp = Stamp::from_nanos(9);
/// The tag of the first entry of the build and of the check. Each entry takes the
/// next tag and fills its bytes with it, so a read cannot give one entry for another.
/// The tags of the build start again after 128 entries, below the tags of the check.
const BUILD_TAG: u8 = 0x01;
const CHECK_TAG: u8 = 0x81;
/// The laps of the area that an offset holds.
const LAPS: u64 = u64::MAX / AREA;

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

/// One entry the build phase appends.
#[derive(Debug)]
struct Append {
    index: usize,
    path: frame::Path,
    len: u32,
    part: usize,
}

/// One batch of the build phase: its appends, then `pad` entries of at most 72
/// bytes, for a table of more than one block.
#[derive(Debug)]
struct Batch {
    appends: Vec<Append>,
    pad: usize,
}

/// One entry of a batch before the build gives it a seq.
struct Planned {
    index: usize,
    path: frame::Path,
    /// Seqs the entry leaves out before its first.
    skip: u64,
    len: u32,
    tag: u8,
    parts: Parts,
}

impl Batch {
    /// The entries of the batch, in order, each with the next of `tags`. The pad
    /// entries differ by their number: samples or a caller record, no part or two,
    /// and a skip ahead.
    fn planned(
        &self,
        pool: &Pool,
        tags: &mut impl Iterator<Item = u8>,
    ) -> Vec<Planned> {
        let block = |len: usize, fill: u8| {
            let mut block = pool.alloc(len).expect("the pool has a block");
            block.fill(fill);
            block.freeze()
        };
        let mut tag = || tags.next().expect("the tags do not end");
        let mut planned = Vec::new();
        for append in &self.appends {
            let tag = tag();
            planned.push(Planned {
                index: append.index,
                path: append.path,
                skip: 0,
                len: append.len,
                tag,
                parts: Parts::from(block(append.part, tag)),
            });
        }
        for number in 0..self.pad {
            let tag = tag();
            planned.push(Planned {
                index: number % INDEXES,
                path: PATHS[number / INDEXES % PATHS.len()],
                skip: if number % 16 == 5 { 2 } else { 0 },
                len: u32::from(number % 4 != 3),
                tag,
                parts: if number % 4 == 1 {
                    Parts::from([block(number % 61, tag), block(number % 13, tag)])
                } else {
                    Parts::default()
                },
            });
        }
        planned
    }
}

/// One change to the file bytes. `MoveTail` counts `laps` under 128 from the first
/// lap, and the rest from the last.
#[derive(Debug)]
enum Edit {
    Put { at: usize, bytes: Vec<u8> },
    Zero { at: usize, len: usize },
    SealHeader { second: bool },
    SealRecord { block: usize },
    MoveTail { block: usize, laps: u8 },
}

#[derive(Debug)]
struct Input {
    /// The replay value of the run, from the input bytes. Bytes on a disk cannot
    /// hold a chain that a later open draws, so an edit must not either.
    replay: u64,
    batches: Vec<Batch>,
    edits: Vec<Edit>,
    /// The check commits a record of two blocks, which a wrap record can come before.
    wide: bool,
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
                    len: u.int_in_range(1..=16)?,
                    part: u.int_in_range(0..=PART_MAX)?,
                });
            }
            batches.push(Batch { appends, pad: 0 });
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
                    laps: u.arbitrary()?,
                },
            });
        }
        // Last, so an input that ends before these has no pad and is not wide.
        for batch in &mut batches {
            batch.pad = u.int_in_range(0..=255)?;
        }
        Ok(Self {
            replay,
            batches,
            edits,
            wide: u.arbitrary()?,
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
            let (len, rest) = image[start..]
                .split_first_chunk::<4>()
                .expect("invariant: a block holds a record header");
            let claimed = u32::from_le_bytes(*len);
            let body = usize::try_from(claimed)
                .expect("invariant: a u32 fits in usize")
                .min(rest.len() - 5);
            let mut crc = crc32c::crc32c_append(chain, len);
            crc = crc32c::crc32c_append(crc, &rest[4..5]);
            crc = crc32c::crc32c_append(crc, &rest[5..5 + body]);
            image[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        }
        Edit::MoveTail { block, laps } => {
            let chain = chain_before(image, *block);
            let lap = match u64::from(*laps) {
                low @ ..128 => low,
                high => LAPS - (255 - high),
            };
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
/// records from the tail in the first header block leave there. The CRCs on the way
/// are not checked. When no record starts at `block`, the value where the way ends.
fn chain_before(image: &[u8], block: usize) -> u32 {
    let at = |offset: usize| -> u32 {
        let bytes = image[offset..offset + 4]
            .try_into()
            .expect("invariant: four bytes");
        u32::from_le_bytes(bytes)
    };
    let tail: [u8; 8] = image[HEADER_TAIL_AT..HEADER_TAIL_AT + 8]
        .try_into()
        .expect("invariant: eight bytes");
    let mut start = 2 + (u64::from_le_bytes(tail) % AREA) as usize / BLOCK;
    let mut chain = at(HEADER_CHAIN_AT);
    for _ in 0..BLOCKS {
        if start == block {
            break;
        }
        let record = start * BLOCK;
        let kind = image[record + 8];
        chain = at(record + if kind == 3 { 9 } else { 4 });
        start = match kind {
            2 => 2,
            _ => start + (9 + at(record) as usize).div_ceil(BLOCK),
        };
        match start.cmp(&(2 + BLOCKS)) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => start = 2,
            std::cmp::Ordering::Greater => break,
        }
    }
    chain
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
/// each path, in slot then path order. An append refuses a batch exactly when
/// `Layout::check` does, and the build goes on after it.
async fn build(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
) -> (Vec<Tail>, Vec<Vec<Given>>) {
    let (buffer, slots) = open(node, tasks, pool).await.expect("a new ring opens");
    let mut next = vec![[0u64; 2]; INDEXES];
    let mut tags = (BUILD_TAG..CHECK_TAG).cycle();
    let mut written: Vec<[Vec<Given>; 2]> =
        iter::repeat_with(Default::default).take(INDEXES).collect();
    for batch in &input.batches {
        let mut firsts = next.clone();
        let mut appended = Vec::new();
        let mut entries = Vec::new();
        let (mut parts, mut bytes) = (0, 0);
        for planned in batch.planned(pool, &mut tags) {
            let path = usize::from(planned.path == frame::Path::Backfill);
            let first = firsts[planned.index][path] + planned.skip;
            firsts[planned.index][path] = first + u64::from(planned.len);
            let last = (planned.len > 0)
                .then(|| Stamp::from_nanos(i64::try_from(first).expect("small")));
            let mut joined = Vec::new();
            for part in planned.parts.clone() {
                parts += 1;
                joined.extend_from_slice(&part);
            }
            bytes += joined.len();
            appended.push((
                planned.index,
                path,
                Given {
                    first,
                    len: planned.len,
                    stored_at: STORED_AT,
                    last,
                    tag: planned.tag,
                    bytes: joined,
                },
            ));
            entries.push(Entry {
                index: key(planned.index),
                slot: slots[planned.index],
                path: planned.path,
                first,
                len: planned.len,
                stored_at: STORED_AT,
                last,
                tag: planned.tag,
                parts: planned.parts,
            });
        }
        let fits = buffer.layout().check(entries.len(), parts, bytes);
        let taken = buffer.append(entries);
        if let Err(limit) = fits {
            assert_eq!(
                taken,
                Err(Rejected::Large(limit)),
                "an append and the layout differ on a batch"
            );
            continue;
        }
        match taken {
            Ok(()) => next = firsts,
            Err(Rejected::Full { .. }) => break,
            Err(other) => panic!("append failed: {other}"),
        }
        buffer.committed().await.expect("the batch commits");
        for (index, path, entry) in appended {
            written[index][path].push(entry);
        }
    }
    let written = written.into_iter().flatten().collect();
    (tails(&buffer, &slots), written)
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
    let sealed = image[..BLOCK].to_vec();
    apply(&mut image, &Edit::SealHeader { second: false });
    assert_eq!(
        image[..BLOCK],
        sealed,
        "the header seal restates the format"
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

/// Opens the changed ring. With no edits, it must give the tails and the entries the
/// build left. When it opens, each path must read, and one commit on it must
/// survive a reopen and read back the same before and after.
///
/// The commit is one entry at each tail, of `CHECK_PART` bytes or, for a wide input,
/// `CHECK_PART_WIDE`. A record edit can put a tail at `u64::MAX`, a precondition of
/// `append`, so such a ring is not checked.
async fn check(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
    built: &[Tail],
    written: &[Vec<Given>],
) {
    let (buffer, slots) = match open(node, tasks, pool).await {
        Ok(opened) => opened,
        Err(Error::Pool(_) | Error::Files(_)) => panic!("open failed outside the ring"),
        Err(_) => return,
    };
    let read = read_paths(&buffer, &slots).await;
    if input.edits.is_empty() {
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
            let len = if input.wide {
                CHECK_PART_WIDE
            } else {
                CHECK_PART
            };
            let mut part = pool.alloc(len).expect("the pool has a block");
            let tag = CHECK_TAG + u8::try_from(commits.len()).expect("few paths");
            part.fill(tag);
            commits.push(Given {
                first: tail.seq,
                len: 1,
                stored_at: STORED_AT,
                last: Some(CHECK_LAST),
                tag,
                bytes: part.to_vec(),
            });
            entries.push(Entry {
                index: key(index),
                slot: *slot,
                path,
                first: tail.seq,
                len: 1,
                stored_at: STORED_AT,
                last: Some(CHECK_LAST),
                tag,
                parts: Parts::from(part.freeze()),
            });
        }
    }
    let before = tails(&buffer, &slots);
    match buffer.append(entries) {
        Ok(()) => {}
        Err(Rejected::Full { .. }) => return,
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
    // An open costs one block for its restart record. No room is full, not lost.
    let (reopened, slots) = match open(node, tasks, pool).await {
        Ok(opened) => opened,
        Err(Error::Full { .. }) => return,
        Err(other) => panic!("a ring with a commit did not reopen: {other}"),
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
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config { budget: 1 << 21 };
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let (built, written) = build(&node, &tasks, &pool, &input).await;
            let file = node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            edit(&file, &pool, &input.edits).await;
            drop(file);
            check(&node, &tasks, &pool, &input, &built, &written).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
