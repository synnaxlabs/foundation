//! `Buffer::open` never panics on a ring file that a local writer changed, and what
//! `committed` reported durable is what a reopen gives.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! Two edits seal a CRC: a header block (over its first 4092 bytes) and a record
//! (over `len`, `kind`, and the body, continued from a chain value). They restate
//! the formats in `header.rs` and `record.rs` of `buffer`; when either moves, the
//! edits stop reaching the walk and coverage drops without a failed replay.

#![no_main]

use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{Buffer, Config, Entry, Error, Layout, Tail};
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
/// The two header blocks come before the area.
const FILE_LEN: usize = (2 + BLOCKS) * BLOCK;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const COMMIT: Span = Span::from_nanos(10_000_000);
const INDEXES: usize = 3;
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];

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
    SealRecord { block: usize, chain: u32 },
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
                    chain: u.arbitrary()?,
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
            let crc = crc32c::crc32c(&image[start..start + BLOCK - 4]);
            image[start + BLOCK - 4..start + BLOCK].copy_from_slice(&crc.to_le_bytes());
        }
        Edit::SealRecord { block, chain } => {
            let start = block * BLOCK;
            let (len, rest) = image[start..]
                .split_first_chunk::<4>()
                .expect("invariant: a block holds a record header");
            let claimed = u32::from_le_bytes(*len);
            let body = usize::try_from(claimed)
                .expect("invariant: a u32 fits in usize")
                .min(rest.len() - 5);
            let mut crc = crc32c::crc32c_append(*chain, len);
            crc = crc32c::crc32c_append(crc, &rest[4..5]);
            crc = crc32c::crc32c_append(crc, &rest[5..5 + body]);
            image[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        }
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

/// Writes the batches to a new ring.
async fn build(node: &sim::node::Node, tasks: &Tasks, pool: &Rc<Pool>, input: &Input) {
    let mut slots = Slots::new();
    let buffer = open(node, tasks, pool, &mut slots)
        .await
        .expect("a new ring opens");
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let mut next = vec![[0u64; 2]; INDEXES];
    for batch in &input.batches {
        let parts: Vec<Block> = batch
            .iter()
            .map(|append| {
                let mut part = pool.alloc(append.part).expect("the pool has a block");
                part.fill(0x5a);
                part.freeze()
            })
            .collect();
        let mut firsts = next.clone();
        let entries: Vec<Entry<'_>> = batch
            .iter()
            .zip(&parts)
            .map(|(append, part)| {
                let path = usize::from(append.path == frame::Path::Backfill);
                let first = firsts[append.index][path];
                firsts[append.index][path] += u64::from(append.len);
                Entry {
                    index: key(append.index),
                    slot: slot_of[append.index],
                    path: append.path,
                    first,
                    len: append.len,
                    stored_at: Stamp::from_nanos(7),
                    last: Some(Stamp::from_nanos(i64::try_from(first).expect("small"))),
                    tag: 0,
                    parts: std::slice::from_ref(part),
                }
            })
            .collect();
        match buffer.append(&entries) {
            Ok(()) => next = firsts,
            Err(Error::Full { .. }) => break,
            Err(other) => panic!("append failed: {other}"),
        }
        buffer.committed().await.expect("the batch commits");
    }
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

/// Opens the changed ring. When it opens, one commit on it must survive a reopen.
///
/// The commit is one empty entry at each tail. A header edit can shrink `body_max`
/// under that batch, and a record edit can put a tail at `u64::MAX`; both are
/// preconditions of `append`, so such a ring is not checked.
async fn check(node: &sim::node::Node, tasks: &Tasks, pool: &Rc<Pool>) {
    let mut slots = Slots::new();
    let Ok(buffer) = open(node, tasks, pool, &mut slots).await else {
        return;
    };
    if buffer.layout().body_max() < BODY_MAX {
        return;
    }
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let mut entries = Vec::new();
    for (index, slot) in slot_of.iter().enumerate() {
        for path in PATHS {
            let tail = buffer.tail(*slot, path);
            if tail.seq == u64::MAX {
                return;
            }
            entries.push(Entry {
                index: key(index),
                slot: *slot,
                path,
                first: tail.seq,
                len: 1,
                stored_at: Stamp::from_nanos(7),
                last: Some(Stamp::from_nanos(9)),
                tag: 0,
                parts: &[],
            });
        }
    }
    match buffer.append(&entries) {
        Ok(()) => {}
        Err(Error::Full { .. }) => return,
        Err(other) => panic!("append failed: {other}"),
    }
    buffer.committed().await.expect("the entries commit");
    let durable: Vec<Tail> = slot_of
        .iter()
        .flat_map(|slot| PATHS.map(|path| buffer.durable(*slot, path)))
        .collect();
    drop(buffer);
    let mut slots = Slots::new();
    // An open costs one block for its restart record. No room is full, not lost.
    let reopened = match open(node, tasks, pool, &mut slots).await {
        Ok(reopened) => reopened,
        Err(Error::Full { .. }) => return,
        Err(other) => panic!("a ring with a commit did not reopen: {other}"),
    };
    let recovered: Vec<Tail> = (0..INDEXES)
        .flat_map(|index| {
            let slot = slots.assign(key(index));
            PATHS.map(|path| reopened.tail(slot, path))
        })
        .collect();
    assert_eq!(recovered, durable, "a reopen lost what committed reported");
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
            build(&node, &tasks, &pool, &input).await;
            let file = node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            edit(&file, &pool, &input.edits).await;
            drop(file);
            check(&node, &tasks, &pool).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
