//! `Buffer::open` never panics on a ring file that a local writer changed, and what
//! `committed` reported durable is what a reopen gives.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! Two edits seal a CRC: a header block (over its first 4092 bytes) and a record
//! (over `len`, `kind`, and the body, continued from a chain value). They restate
//! the formats in `header.rs` and `record.rs` of `buffer`; when either moves, the
//! edits stop reaching the walk and coverage drops without a failed replay.

#![no_main]

use std::path::{Path as FilePath, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{Buffer, Config, Entry, Error, Layout, Tail};
use env::files::Mode;
use env::tasks::Tasks;
use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use types::channel::{self, Slot, Slots};
use types::frame::Path;
use types::time::{Span, Stamp};

const BLOCK: usize = 4096;
/// Records of one block, with room to wrap.
const BLOCKS: usize = 8;
/// A body that keeps a record in one block.
const BODY_MAX: usize = 4087;
/// The two header blocks come before the area.
const FILE_LEN: usize = (2 + BLOCKS) * BLOCK;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const COMMIT: Span = Span::from_nanos(10_000_000);
const INDEXES: u32 = 3;
const PATHS: [Path; 2] = [Path::Live, Path::Backfill];

/// One entry the build phase appends.
#[derive(Debug)]
struct Append {
    index: u32,
    path: Path,
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
struct Ops {
    batches: Vec<Vec<Append>>,
    edits: Vec<Edit>,
}

impl<'a> Arbitrary<'a> for Ops {
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
            let claimed =
                u32::from_le_bytes(image[start..start + 4].try_into().unwrap());
            let len = usize::try_from(claimed)
                .unwrap()
                .min(image.len() - start - 9);
            let mut crc = crc32c::crc32c_append(*chain, &image[start..start + 4]);
            crc = crc32c::crc32c_append(crc, &image[start + 8..start + 9]);
            crc = crc32c::crc32c_append(crc, &image[start + 9..start + 9 + len]);
            image[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        }
    }
}

fn key(index: u32) -> channel::Key {
    channel::Key::from_u128(u128::from(index))
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
        layout: Layout::new(u64::try_from(BLOCKS * BLOCK).unwrap(), BODY_MAX)
            .expect("the sizes make a ring"),
        commit: COMMIT,
    };
    Buffer::open(config, slots).await
}

/// Writes the batches, then applies the edits to the file.
async fn build(node: &sim::node::Node, tasks: &Tasks, pool: &Rc<Pool>, ops: &Ops) {
    let mut slots = Slots::new();
    let buffer = open(node, tasks, pool, &mut slots)
        .await
        .expect("a new ring opens");
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let mut next = vec![[0u64; 2]; slot_of.len()];
    for batch in &ops.batches {
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
                let at = usize::try_from(append.index).unwrap();
                let path = usize::from(append.path == Path::Backfill);
                let first = firsts[at][path];
                firsts[at][path] += u64::from(append.len);
                Entry {
                    index: key(append.index),
                    slot: slot_of[at],
                    path: append.path,
                    first,
                    len: append.len,
                    stored_at: Stamp::from_nanos(7),
                    last: Some(Stamp::from_nanos(i64::try_from(first).unwrap())),
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
    drop(buffer);
    let file = node
        .files()
        .open(FilePath::new(RING), Mode::Write)
        .await
        .expect("the ring is there");
    let mut image = Vec::with_capacity(FILE_LEN);
    for block in 0..FILE_LEN / BLOCK {
        let into = pool.alloc(BLOCK).expect("the pool has a block");
        let read = file
            .read_at(u64::try_from(block * BLOCK).unwrap(), into)
            .await
            .expect("the ring reads");
        image.extend_from_slice(&read);
    }
    for edit in &ops.edits {
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
async fn check(node: &sim::node::Node, tasks: &Tasks, pool: &Rc<Pool>) {
    let mut slots = Slots::new();
    let Ok(buffer) = open(node, tasks, pool, &mut slots).await else {
        return;
    };
    let slot_of: Vec<Slot> =
        (0..INDEXES).map(|index| slots.assign(key(index))).collect();
    let mut entries = Vec::new();
    for (index, slot) in slot_of.iter().enumerate() {
        for path in PATHS {
            let tail = buffer.tail(*slot, path);
            entries.push(Entry {
                index: key(u32::try_from(index).unwrap()),
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

fuzz_target!(|ops: Ops| {
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
            build(&node, &tasks, &pool, &ops).await;
            check(&node, &tasks, &pool).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
