//! Tests of `buffer` through its public surface, on one shard of a simulated node.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod memory;

use std::path::{Path as FilePath, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{Buffer, Config, Entry, Error, Layout, Tail, Unfit};
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::{Error as FileError, Mode, Operation};
use env::tasks::Tasks;
use proptest::prelude::*;
use types::channel::{self, Slot, Slots};
use types::frame::Path;
use types::time::{Span, Stamp};

use crate::memory::Memory;

const BLOCK: u64 = 4096;
/// Sixteen records of one block.
const AREA: u64 = 16 * BLOCK;
/// A body that keeps a record in one block.
const BODY_MAX: usize = 4087;
/// The two header blocks come before the area.
const AREA_START: u64 = 2 * BLOCK;
const COMMIT: Span = Span::from_nanos(10_000_000);
const POOL: usize = 1 << 21;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
/// Where a header block keeps its version, its `body_max`, and its CRC.
const VERSION_AT: usize = 8;
const BODY_MAX_AT: usize = 18;
const CRC_AT: usize = 4092;

/// What one test gets on its shard.
struct Shard {
    memory: Memory,
    pool: Rc<Pool>,
    clock: Clock,
    tasks: Tasks,
    entropy: Entropy,
}

impl Shard {
    async fn open(&self, layout: Layout, slots: &mut Slots) -> Result<Buffer, Error> {
        let config = Config {
            files: self.memory.files(),
            dir: PathBuf::from(DIR),
            pool: Rc::clone(&self.pool),
            clock: self.clock.clone(),
            tasks: self.tasks.clone(),
            entropy: self.entropy.clone(),
            layout,
            commit: COMMIT,
        };
        Buffer::open(config, slots).await
    }

    /// A block of `len` bytes that count up from `len`.
    fn block(&self, len: usize) -> Block {
        let mut block = self.pool.alloc(len).expect("the pool has a block");
        for (at, byte) in block.iter_mut().enumerate() {
            *byte = u8::try_from((len + at) % 251).expect("under 251");
        }
        block.freeze()
    }

    /// Makes the ring file with `len` zero bytes and no header.
    async fn zeroed(&self, len: u64) {
        self.memory
            .files()
            .open(FilePath::new(RING), Mode::Create { len })
            .await
            .expect("the file is made");
    }

    /// Puts `bytes` at `at` of both header blocks and fixes their CRCs.
    fn tamper(&self, at: usize, bytes: &[u8]) {
        let file = self.memory.bytes(RING);
        for place in [0, to_usize(BLOCK)] {
            let mut block = file[place..place + to_usize(BLOCK)].to_vec();
            block[at..at + bytes.len()].copy_from_slice(bytes);
            let crc = crc32c::crc32c(&block[..CRC_AT]);
            block[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
            self.memory.put(RING, place, &block);
        }
    }

    /// Puts `bytes` at `at` of the body of the record at `offset` of the area,
    /// and fixes the record's CRC so that it still follows the restart record in
    /// the block before it.
    fn tamper_record(&self, offset: u64, at: usize, bytes: &[u8]) {
        let file = self.memory.bytes(RING);
        let start = to_usize(AREA_START + offset);
        let u32_at = |at: usize| {
            u32::from_le_bytes(file[at..at + 4].try_into().expect("four bytes"))
        };
        let len = to_usize(u64::from(u32_at(start)));
        let chain = u32_at(start - to_usize(BLOCK) + 9);
        let mut body = file[start + 9..start + 9 + len].to_vec();
        body[at..at + bytes.len()].copy_from_slice(bytes);
        let mut crc = crc32c::crc32c_append(chain, &file[start..start + 4]);
        crc = crc32c::crc32c_append(crc, &file[start + 8..start + 9]);
        crc = crc32c::crc32c_append(crc, &body);
        self.memory.put(RING, start + 4, &crc.to_le_bytes());
        self.memory.put(RING, start + 9, &body);
    }
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("fits in usize")
}

/// `halves` half commit spans.
/// `count` commit spans, where `count` is in halves: `commits(21)` is ten and a
/// half.
fn commits(halves: i64) -> Span {
    Span::from_nanos(COMMIT.nanos() / 2 * halves)
}

fn layout(area: u64, body_max: usize) -> Layout {
    Layout::new(area, body_max).expect("the sizes make a ring")
}

fn key(index: u32) -> channel::Key {
    channel::Key::from_u128(u128::from(index))
}

fn entry(
    index: u32,
    slot: Slot,
    path: Path,
    first: u64,
    len: u32,
    last: Option<i64>,
    parts: &[Block],
) -> Entry<'_> {
    Entry {
        index: key(index),
        slot,
        path,
        first,
        len,
        stored_at: Stamp::from_nanos(7),
        last: last.map(Stamp::from_nanos),
        tag: 0,
        parts,
    }
}

fn tail(seq: u64, stamp: Option<i64>) -> Tail {
    Tail {
        seq,
        stamp: stamp.map(Stamp::from_nanos),
    }
}

/// Starts one shard of one node, with the node's files in `memory`, to run `main`
/// until it returns. A panic in `main` comes back from the run as
/// [`sim::Error::Panicked`].
fn start<F>(
    seed: u64,
    memory: Memory,
    main: impl FnOnce(Shard) -> F + Send + 'static,
) -> (sim::Sim, env::thread::Handle)
where
    F: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let entropy = node.entropy();
    let config = env::shards::Config {
        name: DIR.into(),
        core: None,
    };
    let handle = node
        .shards()
        .start(config, move |tasks| {
            let config = block::Config { budget: POOL };
            let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
            main(Shard {
                memory,
                pool: Rc::new(pool),
                clock,
                tasks,
                entropy,
            })
        })
        .expect("the shard starts");
    (sim, handle)
}

/// Runs `main` on one shard of one node, with the node's files in `memory`, until
/// it returns.
fn run<F>(seed: u64, memory: Memory, main: impl FnOnce(Shard) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    let (mut sim, handle) = start(seed, memory, main);
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}

#[test]
fn a_new_ring_keeps_its_layout_across_opens() {
    let memory = Memory::default();
    run(1, memory.clone(), |shard| async move {
        let first = layout(AREA, BODY_MAX);
        let buffer = shard.open(first, &mut Slots::new()).await.expect("opens");
        assert_eq!(buffer.layout(), first);
        assert_eq!(shard.memory.syncs(), 1, "the header is synced");
        drop(buffer);
        let other = layout(2 * AREA, 2 * BODY_MAX);
        let buffer = shard.open(other, &mut Slots::new()).await.expect("reopens");
        assert_eq!(buffer.layout(), first);
        assert_eq!(shard.memory.syncs(), 1, "a reopen syncs nothing");
    });
    let bytes = memory.bytes(RING);
    assert_eq!(bytes.len(), to_usize(AREA_START + AREA));
    assert_eq!(&bytes[..8], b"FNDNRING");
    assert_eq!(
        bytes[..to_usize(BLOCK)],
        bytes[to_usize(BLOCK)..to_usize(AREA_START)]
    );
}

#[test]
fn entries_are_durable_at_committed_and_recovered_at_open() {
    run(2, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let b = slots.assign(key(2));
        let parts = [shard.block(100)];
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &parts)])
            .expect("queues");
        buffer
            .append(&[
                entry(2, b, Path::Backfill, 5, 2, Some(7), &parts),
                entry(1, a, Path::Live, 3, 1, None, &[]),
            ])
            .expect("queues");
        let live = tail(4, Some(30));
        let backfill = tail(7, Some(7));
        assert_eq!(buffer.tail(a, Path::Live), live);
        assert_eq!(buffer.tail(b, Path::Backfill), backfill);
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(buffer.durable(b, Path::Backfill), Tail::default());
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), live);
        assert_eq!(buffer.durable(b, Path::Backfill), backfill);
        assert_eq!(shard.memory.syncs(), 2, "one sync per commit");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        let b = slots.assign(key(2));
        let a = slots.assign(key(1));
        assert_eq!(buffer.tail(a, Path::Live), live);
        assert_eq!(buffer.durable(a, Path::Live), live);
        assert_eq!(buffer.tail(b, Path::Backfill), backfill);
        assert_eq!(buffer.tail(a, Path::Backfill), Tail::default());
        assert_eq!(buffer.tail(b, Path::Live), Tail::default());
    });
}

#[test]
fn durable_moves_only_at_a_commit() {
    run(3, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append(&[entry(1, a, Path::Live, 3, 2, Some(50), &[])])
            .expect("queues");
        assert_eq!(buffer.tail(a, Path::Live), tail(5, Some(50)));
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(5, Some(50)));
    });
}

#[test]
fn an_idle_buffer_wakes_no_task() {
    let (mut sim, handle) = start(20, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(2000)).await;
        drop(buffer);
    });
    sim.run_for(COMMIT).expect("the open ends");
    let idle = sim.digest();
    sim.run_for(commits(200)).expect("the buffer idles");
    assert_eq!(sim.digest(), idle, "a task ran while the buffer idled");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}

#[test]
fn committed_on_an_idle_buffer_resolves_after_one_commit() {
    run(21, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(21)).await;
        let before = shard.clock.now();
        buffer.committed().await.expect("commits");
        assert_eq!(shard.clock.now() - before, COMMIT);
    });
}

#[test]
fn the_first_append_after_an_idle_span_commits_after_one_commit() {
    run(22, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        shard.clock.sleep(commits(21)).await;
        let before = shard.clock.now();
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(shard.clock.now() - before, COMMIT);
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
    });
}

#[test]
fn a_dropped_idle_buffer_ends_its_task_at_once() {
    let memory = Memory::default();
    run(23, memory.clone(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(21)).await;
        assert_eq!(shard.memory.open_files(), 1);
        drop(buffer);
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 4))
            .await;
        assert_eq!(shard.memory.open_files(), 0, "the task holds the ring open");
    });
}

#[test]
fn a_batch_past_the_open_group_starts_the_next_record() {
    run(4, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let first = [shard.block(2000)];
        let rest = [shard.block(1500)];
        buffer
            .append(&[entry(1, a, Path::Live, 0, 1, Some(1), &first)])
            .expect("queues");
        buffer
            .append(&[
                entry(1, a, Path::Live, 1, 1, Some(2), &rest),
                entry(1, a, Path::Live, 2, 1, Some(3), &rest),
            ])
            .expect("the batch starts the next record");
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(3)));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(3)));
        assert_eq!(shard.memory.syncs(), 2, "both records go in one sync");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.assign(key(1)), Path::Live),
            tail(3, Some(3))
        );
    });
}

#[test]
fn a_full_ring_queues_nothing() {
    run(5, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(3 * BLOCK, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let parts = [shard.block(3900)];
        buffer
            .append(&[entry(1, a, Path::Live, 0, 1, None, &parts)])
            .expect("the first record has room");
        buffer
            .append(&[entry(1, a, Path::Live, 1, 1, None, &parts)])
            .expect("the second record has room");
        let full = buffer.append(&[entry(1, a, Path::Live, 2, 1, None, &parts)]);
        assert_eq!(
            full,
            Err(Error::Full {
                needed: 4096,
                free: 0
            })
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(2, None));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, None));
    });
}

#[test]
fn a_batch_is_queued_whole_or_not_at_all() {
    run(6, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(3 * BLOCK, 8183), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let parts = [shard.block(3900)];
        buffer
            .append(&[entry(1, a, Path::Live, 0, 1, None, &parts)])
            .expect("the first record has room");
        let full = buffer.append(&[
            entry(1, a, Path::Live, 1, 1, None, &parts),
            entry(1, a, Path::Live, 2, 1, None, &parts),
        ]);
        assert_eq!(
            full,
            Err(Error::Full {
                needed: 12288,
                free: 4096
            }),
            "the first entry alone has room, the batch does not"
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(1, None));
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(3 * BLOCK, 8183), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(buffer.tail(slots.assign(key(1)), Path::Live), tail(1, None));
    });
}

#[test]
fn a_failed_sync_ends_the_buffer_with_its_error() {
    run(7, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        shard.memory.fail_syncs();
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        let failed = Err(Error::Files(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        }));
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(
            buffer.append(&[entry(1, a, Path::Live, 3, 1, None, &[])]),
            failed
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(shard.memory.syncs(), 2, "the task ended at the failed sync");
    });
}

#[test]
fn a_drop_ends_the_commit_task() {
    run(8, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(shard.memory.syncs(), 2);
        buffer
            .append(&[entry(1, a, Path::Live, 3, 1, None, &[])])
            .expect("queues");
        drop(buffer);
        shard.clock.sleep(commits(10)).await;
        assert_eq!(shard.memory.syncs(), 2, "no deadline runs after the drop");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.assign(key(1)), Path::Live),
            tail(3, Some(30)),
            "the entry queued at the drop was not written"
        );
    });
}

#[test]
fn committed_waits_for_an_entry_appended_during_a_commit() {
    run(19, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let sync = Span::from_nanos(4_000_000);
        shard.memory.slow_syncs(shard.clock.clone(), sync);
        buffer
            .append(&[entry(1, a, Path::Live, 0, 1, Some(1), &[])])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append(&[entry(1, a, Path::Live, 1, 1, Some(2), &[])])
            .expect("queues while the first sync runs");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, Some(2)));
        assert_eq!(
            shard.memory.syncs(),
            3,
            "the second entry took its own sync"
        );
    });
}

#[test]
fn an_entry_below_the_tail_is_a_broken_invariant() {
    let (mut sim, _handle) = start(9, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, None, &[])])
            .expect("queues");
        drop(buffer.append(&[entry(1, a, Path::Live, 2, 1, None, &[])]));
    });
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: DIR.into(),
            message: "invariant: an entry of index \
                      00000000-0000-0000-0000-000000000001 on path Live starts at 2, \
                      below the tail 3"
                .into(),
            seed: 9,
        })
    );
}

#[test]
fn a_zeroed_file_of_another_length_is_not_made_into_a_ring() {
    run(10, Memory::default(), |shard| async move {
        shard.zeroed(AREA_START + AREA + BLOCK).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Length {
                expected: AREA_START + AREA,
                found: AREA_START + AREA + BLOCK,
            })
        );
    });
}

#[test]
fn a_file_shorter_than_the_header_blocks_is_not_read() {
    run(17, Memory::default(), |shard| async move {
        shard.zeroed(BLOCK).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Length {
                expected: AREA_START + AREA,
                found: BLOCK,
            })
        );
    });
}

#[test]
fn a_file_with_no_header_is_missing() {
    run(11, Memory::default(), |shard| async move {
        shard.zeroed(AREA_START + AREA).await;
        shard.memory.put(RING, 0, b"not a ring");
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Missing));
    });
}

#[test]
fn two_header_blocks_with_a_wrong_crc_are_damaged() {
    run(12, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.memory.put(RING, BODY_MAX_AT, &[1]);
        shard.memory.put(RING, to_usize(BLOCK) + BODY_MAX_AT, &[1]);
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Damaged));
    });
}

#[test]
fn another_format_version_is_not_read() {
    run(13, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.tamper(VERSION_AT, &2_u16.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Version(2)));
    });
}

#[test]
fn a_header_with_sizes_that_make_no_ring_is_unfit() {
    run(14, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.tamper(BODY_MAX_AT, &0_u32.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Unfit(Unfit {
                area: AREA,
                body_max: 0
            }))
        );
    });
}

#[test]
fn a_record_whose_entry_cannot_be_read_is_invalid() {
    run(18, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        shard.tamper_record(BLOCK, 4 + 16, &[2]);
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Invalid { offset: BLOCK }));
    });
}

#[test]
fn each_open_starts_a_new_chain() {
    run(20, Memory::default(), |shard| async move {
        for _ in 0..2 {
            let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
            drop(buffer.expect("opens"));
        }
        let file = shard.memory.bytes(RING);
        let chain = |offset: u64| {
            let at = to_usize(AREA_START + offset) + 9;
            u32::from_le_bytes(file[at..at + 4].try_into().expect("four bytes"))
        };
        assert_ne!(chain(0), chain(BLOCK), "both opens drew the same chain");
    });
}

#[test]
fn a_full_ring_does_not_reopen_before_its_tail_moves() {
    run(21, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(3 * BLOCK, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let parts = [shard.block(3900)];
        for seq in 0..2 {
            buffer
                .append(&[entry(1, a, Path::Live, seq, 1, None, &parts)])
                .expect("the record has room");
        }
        buffer.committed().await.expect("commits");
        drop(buffer);
        let opened = shard
            .open(layout(3 * BLOCK, BODY_MAX), &mut Slots::new())
            .await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Full {
                needed: 4096,
                free: 0
            })
        );
    });
}

#[test]
fn an_error_says_what_went_wrong() {
    let pool = Error::Pool(block::Error::TooLarge {
        requested: 1,
        largest: 0,
    });
    let files = Error::Files(FileError::NotFound {
        path: PathBuf::from(RING),
    });
    let unfit = Error::Unfit(Unfit {
        area: 1,
        body_max: 2,
    });
    let texts = [
        (
            Error::Full { needed: 1, free: 0 },
            "the ring has no room for the batch: it needs 1 bytes and 0 are free",
        ),
        (
            pool,
            "the pool has no block: block of 1 bytes is above the largest block of \
             0 bytes",
        ),
        (files, "a file call failed: path shard-0/ring is not there"),
        (
            Error::Length {
                expected: 1,
                found: 2,
            },
            "the ring file is 2 bytes long, and its ring is 1",
        ),
        (
            Error::Missing,
            "the ring file has no header: it is not a ring",
        ),
        (
            Error::Damaged,
            "the ring is lost: both header blocks have a wrong CRC",
        ),
        (
            Error::Version(3),
            "the ring has format version 3, which this build does not read",
        ),
        (
            unfit,
            "the ring header holds an area of 1 bytes and a body of at most 2 \
             bytes, which make no ring",
        ),
        (
            Error::Invalid { offset: 4 },
            "the ring holds bytes at 4 that this build cannot read",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}

/// One step of a model run.
#[derive(Clone, Debug)]
enum Step {
    Append {
        index: u32,
        path: Path,
        skip: u64,
        len: u32,
        bytes: usize,
        last: Option<i64>,
    },
    Commit,
    Reopen,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (
            0..3_u32,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..3_u64,
            0..5_u32,
            0..200_usize,
            prop::option::of(any::<i64>()),
        )
            .prop_map(|(index, path, skip, len, bytes, last)| Step::Append {
                index,
                path,
                skip,
                len,
                bytes,
                last,
            }),
        2 => Just(Step::Commit),
        1 => Just(Step::Reopen),
    ]
}

type Model = types::hash::Map<(u32, Path), Tail>;

/// Runs the steps against a model: `tails` moves at each append, `durable` at each
/// commit, and a reopen keeps only what was committed.
async fn follow(shard: Shard, steps: Vec<Step>) {
    let large = layout(256 * BLOCK, BODY_MAX);
    let mut slots = Slots::new();
    let mut buffer = shard.open(large, &mut slots).await.expect("opens");
    let mut tails = Model::default();
    let mut durable = Model::default();
    for step in steps {
        match step {
            Step::Append {
                index,
                path,
                skip,
                len,
                bytes,
                last,
            } => {
                let slot = slots.assign(key(index));
                let at = tails.entry((index, path)).or_default();
                let first = at.seq + skip;
                let parts = [shard.block(bytes)];
                buffer
                    .append(&[entry(index, slot, path, first, len, last, &parts)])
                    .expect("the ring has room");
                at.seq = first + u64::from(len);
                if let Some(last) = last {
                    at.stamp = Some(Stamp::from_nanos(last));
                }
            }
            Step::Commit => {
                buffer.committed().await.expect("commits");
                durable.clone_from(&tails);
            }
            Step::Reopen => {
                drop(buffer);
                slots = Slots::new();
                buffer = shard.open(large, &mut slots).await.expect("reopens");
                tails.clone_from(&durable);
            }
        }
        for index in 0..3 {
            for path in [Path::Live, Path::Backfill] {
                let slot = slots.assign(key(index));
                let expected = tails.get(&(index, path)).copied().unwrap_or_default();
                assert_eq!(
                    buffer.tail(slot, path),
                    expected,
                    "tail of {index} {path:?}"
                );
                let expected = durable.get(&(index, path)).copied().unwrap_or_default();
                assert_eq!(
                    buffer.durable(slot, path),
                    expected,
                    "durable of {index} {path:?}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn follows_a_model_across_commits_and_reopens(
        seed in any::<u64>(),
        steps in prop::collection::vec(step(), 0..40),
    ) {
        run(seed, Memory::default(), |shard| follow(shard, steps));
    }
}

#[test]
fn a_drop_right_after_the_first_append_of_an_idle_span_ends_the_task_at_once() {
    run(40, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        shard.clock.sleep(commits(21)).await;
        buffer
            .append(&[entry(1, a, Path::Live, 0, 3, Some(30), &[])])
            .expect("queues");
        drop(buffer);
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 4))
            .await;
        assert_eq!(shard.memory.open_files(), 0, "the task ended");
        shard.clock.sleep(commits(10)).await;
        assert_eq!(shard.memory.syncs(), 1, "no deadline runs after the drop");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.assign(key(1)), Path::Live),
            tail(0, None),
            "the entry queued at the drop was not written"
        );
    });
}

#[test]
fn a_drop_during_a_commit_ends_the_task_after_the_sync() {
    run(41, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let sync = Span::from_nanos(4_000_000);
        shard.memory.slow_syncs(shard.clock.clone(), sync);
        buffer
            .append(&[entry(1, a, Path::Live, 0, 1, Some(1), &[])])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append(&[entry(1, a, Path::Live, 1, 1, Some(2), &[])])
            .expect("queues while the first sync runs");
        drop(buffer);
        assert_eq!(
            shard.memory.open_files(),
            1,
            "the task syncs the first entry"
        );
        shard.clock.sleep(sync).await;
        assert_eq!(
            shard.memory.open_files(),
            0,
            "the task ended after the sync"
        );
        shard.clock.sleep(commits(10)).await;
        assert_eq!(shard.memory.syncs(), 2, "no deadline runs after the drop");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.assign(key(1)), Path::Live),
            tail(1, Some(1)),
            "the entry queued at the drop was not written"
        );
    });
}
