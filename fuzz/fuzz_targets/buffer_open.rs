//! `Buffer::open` never panics on a ring file that a local writer changed, what
//! `committed` reported durable is what a reopen gives, and `Buffer::read` gives
//! each path of an open ring by the read rules, the same in one read and in steps.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! Two edits seal a CRC: a header block (at offset 42, over its first 512-byte
//! sector less the CRC) and a record (over `len`, `kind`, and the body, continued
//! from the chain the block before leaves: the header's chain field, a restart
//! record's body, or a record's CRC).
//! They restate the formats in `header.rs` and `record.rs` of `buffer`. When the
//! header moves, the seal of the first block as written changes it and the target
//! panics. When the record moves, the edits stop reaching the walk and coverage
//! drops without a failed replay.

#![no_main]

use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{Buffer, Config, Entry, Error, Layout, Mark, Parts, Rejected, Tail};
use env::files::{File, Mode};
use env::tasks::Tasks;
use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use types::channel::{self, Slot, Slots};
use types::frame;
use types::time::{Span, Stamp};

const BLOCK: usize = 4096;
/// Blocks in the area: one record each, with room for the build, the check, and
/// the restart record each open writes.
const BLOCKS: usize = 8;
const AREA: u64 = (BLOCKS * BLOCK) as u64;
/// A record is 9 bytes of header and a body, so this keeps a record in one block.
const BODY_MAX: usize = BLOCK - 9;
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
/// Bytes of each entry the check commits.
const CHECK_PART: usize = 512;
/// The check's batch and its table, of 51 bytes an entry, fit one record.
const _: () = assert!(4 + INDEXES * PATHS.len() * (51 + CHECK_PART) <= BODY_MAX);
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];
/// More reads than the area holds entries, at 51 table bytes an entry.
const READS_MAX: usize = BLOCKS * BODY_MAX / 51 + 1;
/// What the build and the check put in each entry.
const STORED_AT: Stamp = Stamp::from_nanos(7);
const CHECK_LAST: Stamp = Stamp::from_nanos(9);
const CHECK_FILL: u8 = 0xa5;
const BUILD_FILL: u8 = 0x5a;

/// One entry of a path, as appended or as a read gave it, with its bytes off the
/// pool.
#[derive(Debug, PartialEq, Eq)]
struct Given {
    first: u64,
    len: u32,
    stored_at: Stamp,
    last: Option<Stamp>,
    tag: u8,
    bytes: Vec<u8>,
}

/// One entry the build phase appends.
#[derive(Debug)]
struct Append {
    index: usize,
    path: frame::Path,
    len: u32,
    part: usize,
}

/// One change to the file bytes.
#[derive(Debug)]
enum Edit {
    Put { at: usize, bytes: Vec<u8> },
    Zero { at: usize, len: usize },
    SealHeader { second: bool },
    SealRecord { block: usize },
}

#[derive(Debug)]
struct Input {
    batches: Vec<Vec<Append>>,
    edits: Vec<Edit>,
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let mut batches = Vec::new();
        for _ in 0..u.int_in_range(0..=4)? {
            let mut batch = Vec::new();
            for _ in 0..u.int_in_range(1..=4)? {
                batch.push(Append {
                    index: u.int_in_range(0..=INDEXES - 1)?,
                    path: PATHS[usize::from(u.int_in_range(0..=1u8)?)],
                    len: u.int_in_range(1..=16)?,
                    part: u.int_in_range(0..=512)?,
                });
            }
            batches.push(batch);
        }
        let mut edits = Vec::new();
        for _ in 0..u.int_in_range(0..=16)? {
            edits.push(match u.int_in_range(0..=3u8)? {
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
                _ => Edit::SealRecord {
                    block: u.int_in_range(2..=BLOCKS + 1)?,
                },
            });
        }
        Ok(Self { batches, edits })
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
    }
}

/// The chain value a record at `block` must continue from.
fn chain_before(image: &[u8], block: usize) -> u32 {
    let at = |offset: usize| -> u32 {
        let bytes = image[offset..offset + 4]
            .try_into()
            .expect("invariant: four bytes");
        u32::from_le_bytes(bytes)
    };
    if block == 2 {
        return at(30);
    }
    let before = (block - 1) * BLOCK;
    if image[before + 8] == 3 {
        at(before + 9)
    } else {
        at(before + 4)
    }
}

fn key(index: usize) -> channel::Key {
    channel::Key::from_u128(index as u128)
}

async fn open(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    slots: &mut Slots,
) -> std::result::Result<Buffer, Error> {
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
    Buffer::open(config, slots).await
}

/// Writes the batches to a new ring and returns the durable tail and the entries of
/// each path, in slot then path order.
async fn build(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
) -> (Vec<Tail>, Vec<Vec<Given>>) {
    let mut slots = Slots::new();
    let buffer = open(node, tasks, pool, &mut slots)
        .await
        .expect("a new ring opens");
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let mut next = vec![[0u64; 2]; INDEXES];
    let mut given: Vec<Vec<Given>> = Vec::new();
    given.resize_with(INDEXES * PATHS.len(), Vec::new);
    for batch in &input.batches {
        let mut firsts = next.clone();
        let mut appended = Vec::new();
        let entries: Vec<Entry> = batch
            .iter()
            .map(|append| {
                let mut part = pool.alloc(append.part).expect("the pool has a block");
                part.fill(BUILD_FILL);
                let path = usize::from(append.path == frame::Path::Backfill);
                let first = firsts[append.index][path];
                firsts[append.index][path] += u64::from(append.len);
                let last =
                    Some(Stamp::from_nanos(i64::try_from(first).expect("small")));
                appended.push((
                    append.index * PATHS.len() + path,
                    Given {
                        first,
                        len: append.len,
                        stored_at: STORED_AT,
                        last,
                        tag: 0,
                        bytes: part.to_vec(),
                    },
                ));
                Entry {
                    index: key(append.index),
                    slot: slot_of[append.index],
                    path: append.path,
                    first,
                    len: append.len,
                    stored_at: STORED_AT,
                    last,
                    tag: 0,
                    parts: Parts::from(part.freeze()),
                }
            })
            .collect();
        match buffer.append(entries) {
            Ok(()) => next = firsts,
            Err(Rejected::Full { .. }) => break,
            Err(other) => panic!("append failed: {other}"),
        }
        buffer.committed().await.expect("the batch commits");
        for (place, entry) in appended {
            given[place].push(entry);
        }
    }
    (tails(&buffer, &slot_of), given)
}

/// Every entry of each path from the start, in slot then path order.
async fn read_paths(buffer: &Buffer, slot_of: &[Slot]) -> Vec<Vec<Given>> {
    let mut paths = Vec::new();
    for slot in slot_of {
        for path in PATHS {
            let whole = read_path(buffer, *slot, path, usize::MAX).await;
            let stepped = read_path(buffer, *slot, path, 1).await;
            assert_eq!(stepped, whole, "reads in steps gave other entries");
            paths.push(whole);
        }
    }
    paths
}

/// Every entry of `path` from the start, in reads of `budget`. Each read keeps the
/// read rules: a gap only before its first entry and only when the entry starts
/// past the mark, no gap between its entries, and `next` after its last entry.
async fn read_path(
    buffer: &Buffer,
    slot: Slot,
    path: frame::Path,
    budget: usize,
) -> Vec<Given> {
    let mut given = Vec::new();
    let mut from = Mark::at(0);
    for _ in 0..READS_MAX {
        let read = match buffer.read(slot, path, from, budget).await {
            Ok(read) => read,
            Err(error) => panic!("a read of an open ring failed: {error}"),
        };
        if read.entries.is_empty() {
            assert_eq!(read.gap, None, "a gap with no entry");
            assert_eq!(read.next, from, "an empty read moved the mark");
            return given;
        }
        let mut at = from.seq;
        for (number, entry) in read.entries.iter().enumerate() {
            if number == 0 {
                let gap = (entry.first > at).then_some(at..entry.first);
                assert_eq!(read.gap, gap, "the gap before the first entry");
            } else {
                assert_eq!(entry.first, at, "a gap before entry {number}");
            }
            at = entry
                .first
                .checked_add(u64::from(entry.len))
                .expect("an entry ends past the last seq");
            given.push(Given {
                first: entry.first,
                len: entry.len,
                stored_at: entry.stored_at,
                last: entry.last,
                tag: entry.tag,
                bytes: entry.bytes.to_vec(),
            });
        }
        assert_eq!(read.next.seq, at, "next is not after the last entry");
        from = read.next;
    }
    panic!("the reads of a path did not end");
}

/// The durable tail of each path, in slot then path order.
fn tails(buffer: &Buffer, slot_of: &[Slot]) -> Vec<Tail> {
    slot_of
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
/// The commit is one entry of `CHECK_PART` bytes at each tail. A record edit can put a
/// tail at `u64::MAX`, a precondition of `append`, so such a ring is not checked.
async fn check(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    input: &Input,
    built: &[Tail],
    given: &[Vec<Given>],
) {
    let mut slots = Slots::new();
    let buffer = match open(node, tasks, pool, &mut slots).await {
        Ok(buffer) => buffer,
        Err(Error::Pool(_) | Error::Files(_)) => panic!("open failed outside the ring"),
        Err(_) => return,
    };
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    if input.edits.is_empty() {
        assert_eq!(
            tails(&buffer, &slot_of),
            built,
            "an open lost what the build wrote"
        );
    }
    let read = read_paths(&buffer, &slot_of).await;
    if input.edits.is_empty() {
        assert_eq!(read, given, "a read lost what the build wrote");
    }
    let mut entries = Vec::new();
    let mut wanted = Vec::new();
    for (index, slot) in slot_of.iter().enumerate() {
        for path in PATHS {
            let tail = buffer.tail(*slot, path);
            if tail.seq == u64::MAX {
                return;
            }
            let mut part = pool.alloc(CHECK_PART).expect("the pool has a block");
            part.fill(CHECK_FILL);
            wanted.push(Given {
                first: tail.seq,
                len: 1,
                stored_at: STORED_AT,
                last: Some(CHECK_LAST),
                tag: 0,
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
                tag: 0,
                parts: Parts::from(part.freeze()),
            });
        }
    }
    match buffer.append(entries) {
        Ok(()) => {}
        Err(Rejected::Full { .. }) => return,
        Err(other) => panic!("append failed: {other}"),
    }
    buffer.committed().await.expect("the entries commit");
    let durable = tails(&buffer, &slot_of);
    let stored = read_paths(&buffer, &slot_of).await;
    for (path, wanted) in stored.iter().zip(&wanted) {
        assert_eq!(path.last(), Some(wanted), "a read lost the commit");
    }
    drop(buffer);
    let mut slots = Slots::new();
    // An open costs one block for its restart record. No room is full, not lost.
    let reopened = match open(node, tasks, pool, &mut slots).await {
        Ok(reopened) => reopened,
        Err(Error::Full { .. }) => return,
        Err(other) => panic!("a ring with a commit did not reopen: {other}"),
    };
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let recovered: Vec<Tail> = slot_of
        .iter()
        .flat_map(|slot| PATHS.map(|path| reopened.tail(*slot, path)))
        .collect();
    assert_eq!(recovered, durable, "a reopen lost what committed reported");
    assert_eq!(
        read_paths(&reopened, &slot_of).await,
        stored,
        "a reopen changed what a read gives"
    );
}

fuzz_target!(|input: Input| {
    let mut sim = sim::Sim::new(sim::Config::default());
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
            let (built, given) = build(&node, &tasks, &pool, &input).await;
            let file = node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            edit(&file, &pool, &input.edits).await;
            drop(file);
            check(&node, &tasks, &pool, &input, &built, &given).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
