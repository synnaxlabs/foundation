//! Tests of `buffer` through its public surface, on one shard of a simulated node.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod memory;

use std::future::poll_fn;
use std::ops::Range;
use std::path::{Path as FilePath, PathBuf};
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use block::{Block, Heap, Pool};
use buffer::{Buffer, Config, Entry, Error, Layout, Limit, Parts, Tail, Unfit};
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::{Error as FileError, Mode, Operation, SECTOR};
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
const CRC_AT: usize = 42;
/// The bytes the header CRC covers.
const COVER: usize = 512;

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
            let crc = crc32c::crc32c_append(crc, &block[CRC_AT + 4..COVER]);
            block[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
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
    parts: Parts,
) -> Entry {
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
        let parts = Parts::from(shard.block(100));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), parts.clone())])
            .expect("queues");
        buffer
            .append([
                entry(2, b, Path::Backfill, 5, 2, Some(7), parts.clone()),
                entry(1, a, Path::Live, 3, 1, None, Parts::default()),
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
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())])
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

/// Runs a buffer that idles while its shard wakes every one and a half commits,
/// with `empty` empty appends at each wake. Returns the digest of the run.
fn idle_with_wakes(empty: usize) -> u64 {
    let (mut sim, handle) = start(27, Memory::default(), move |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        for _ in 0..8 {
            shard.clock.sleep(commits(3)).await;
            for _ in 0..empty {
                buffer.append(Vec::new()).expect("takes an empty batch");
            }
        }
        drop(buffer);
    });
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
    sim.digest()
}

/// Empty appends on an idle buffer wake no task: the run goes as one with no
/// appends.
#[test]
fn empty_appends_wake_no_task() {
    assert_eq!(idle_with_wakes(2), idle_with_wakes(0));
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
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        shard.clock.sleep(commits(1)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(0, None));
        shard.clock.sleep(commits(2)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        assert_eq!(shard.memory.syncs(), 2, "the append alone woke the task");
    });
}

#[test]
fn a_busy_buffer_keeps_one_deadline_per_commit() {
    run(42, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let tenths = |count: i64| Span::from_nanos(COMMIT.nanos() / 10 * count);
        shard.memory.slow_syncs(shard.clock.clone(), tenths(2));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(11)).await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues during the first sync");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(shard.memory.syncs(), 3, "two deadlines, one commit apart");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, Some(2)));
    });
}

/// The task parks with a deadline it never polled. A push at that exact instant
/// wakes it, and the deadline that just passed fires at once.
#[test]
fn an_append_at_the_deadline_of_a_parked_task_commits_at_once() {
    run(43, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        shard.clock.sleep(COMMIT).await;
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 10))
            .await;
        assert_eq!(
            shard.memory.syncs(),
            2,
            "the passed deadline fired at the wake"
        );
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
    });
}

/// A deadline that passes while a sync runs fires when the sync ends: the task did not
/// idle, so the entries that came during the sync wait no longer. The task idles once
/// before the first entry, as after an open.
#[test]
fn a_deadline_that_passes_during_a_sync_fires_when_the_sync_ends() {
    for (tenths, seed) in [(10, 45), (11, 46)] {
        run(seed, Memory::default(), move |shard| async move {
            let mut slots = Slots::new();
            let buffer = shard
                .open(layout(AREA, BODY_MAX), &mut slots)
                .await
                .expect("opens");
            let a = slots.assign(key(1));
            let tenth = COMMIT.nanos() / 10;
            let sync = Span::from_nanos(tenth * tenths);
            shard.memory.slow_syncs(shard.clock.clone(), sync);
            shard.clock.sleep(commits(21)).await;
            let opened = shard.clock.now();
            buffer
                .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
                .expect("queues");
            shard.clock.sleep(Span::from_nanos(tenth * 11)).await;
            buffer
                .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
                .expect("queues during the first sync");
            let end = opened + Span::from_nanos(tenth * (10 + 2 * tenths + 1));
            shard.clock.sleep_until(end).await;
            assert_eq!(
                buffer.durable(a, Path::Live),
                tail(2, Some(2)),
                "a sync of {tenths} tenths: the second entry waits for no fresh \
                 deadline"
            );
        });
    }
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
        let first = Parts::from(shard.block(2000));
        let rest = Parts::from(shard.block(1500));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), first.clone())])
            .expect("queues");
        buffer
            .append([
                entry(1, a, Path::Live, 1, 1, Some(2), rest.clone()),
                entry(1, a, Path::Live, 2, 1, Some(3), rest.clone()),
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
        let parts = Parts::from(shard.block(3900));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, None, parts.clone())])
            .expect("the first record has room");
        buffer
            .append([entry(1, a, Path::Live, 1, 1, None, parts.clone())])
            .expect("the second record has room");
        let full = buffer.append([entry(1, a, Path::Live, 2, 1, None, parts.clone())]);
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
            .open(layout(4 * BLOCK, 8183), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let first = Parts::from(shard.block(5000));
        let parts = Parts::from(shard.block(3900));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, None, first)])
            .expect("the first record has room");
        let full = buffer.append([
            entry(1, a, Path::Live, 1, 1, None, parts.clone()),
            entry(1, a, Path::Live, 2, 1, None, parts.clone()),
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
            .open(layout(4 * BLOCK, 8183), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(buffer.tail(slots.assign(key(1)), Path::Live), tail(1, None));
    });
}

/// A batch over a limit of one record is refused whole, and the batches before
/// and after it commit in its place.
#[test]
fn a_batch_no_record_holds_is_large_and_queues_nothing() {
    run(108, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let small = Parts::from(shard.block(10));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, None, small.clone())])
            .expect("the record has room");
        let empty = Parts::from(shard.block(0));
        let two = Parts::from([shard.block(0), shard.block(0)]);
        let body = Parts::from(shard.block(buffer.layout().entry_max() + 1));
        let cases = [
            (
                vec![entry(1, a, Path::Live, 1, 1, None, empty.clone()); 1024],
                Limit::Entries { count: 1024 },
                "the batch has 1024 entries, and a record holds at most 1023",
            ),
            (
                vec![entry(1, a, Path::Live, 1, 1, None, two); 512],
                Limit::Parts { count: 1024 },
                "the batch has 1024 parts, and a record holds at most 1023",
            ),
            (
                vec![entry(1, a, Path::Live, 1, 1, None, body.clone())],
                Limit::Body {
                    len: 4088,
                    max: 4087,
                },
                "the batch needs a record body of 4088 bytes, and a record of this \
                 ring holds at most 4087",
            ),
        ];
        for (batch, limit, message) in cases {
            let large = buffer.append(batch);
            assert_eq!(large, Err(Error::Large(limit)));
            assert_eq!(Error::Large(limit).to_string(), message);
            assert_eq!(buffer.tail(a, Path::Live), tail(1, None));
        }
        buffer
            .append([entry(1, a, Path::Live, 1, 1, None, small.clone())])
            .expect("the record has room");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let count = to_usize(AREA_START + BLOCK) + 9;
        assert_eq!(
            shard.memory.bytes(RING)[count..count + 4],
            2_u32.to_le_bytes(),
            "the large batches left the group open, so one record holds both"
        );
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(buffer.tail(slots.assign(key(1)), Path::Live), tail(2, None));
    });
}

/// A block of a size class only the test uses: two purges give its class back
/// once the append dropped it. Two, because a purge frees a class that was idle
/// at the purge before.
#[test]
fn a_failed_append_holds_no_part() {
    run(109, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        shard.pool.purge();
        shard.pool.purge();
        let before = shard.pool.committed();
        let body = Parts::from(shard.block(200_000));
        let large = buffer.append([entry(1, a, Path::Live, 0, 1, None, body)]);
        assert_eq!(
            large,
            Err(Error::Large(Limit::Body {
                len: 200_055,
                max: BODY_MAX,
            }))
        );
        shard.pool.purge();
        assert!(
            shard.pool.purge() > 0,
            "the class of the part has no block in use"
        );
        assert_eq!(shard.pool.committed(), before, "the part went back");
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
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = Err(Error::Files(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        }));
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(
            buffer.append([entry(1, a, Path::Live, 3, 1, None, Parts::default())]),
            failed
        );
        assert_eq!(buffer.append(Vec::new()), failed, "an empty append");
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(shard.memory.syncs(), 2, "the task ended at the failed sync");
    });
}

/// `commits` counts the commits that ended: each one that a `committed` future
/// waited on, and one with nothing to write. A failed commit does not count.
#[test]
fn commits_counts_the_commits_that_ended() {
    run(29, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        assert_eq!(buffer.commits(), 0);
        for (count, first, last) in [(1, 0, 30), (2, 3, 60)] {
            buffer
                .append([entry(
                    1,
                    a,
                    Path::Live,
                    first,
                    3,
                    Some(last),
                    Parts::default(),
                )])
                .expect("queues");
            buffer.committed().await.expect("commits");
            assert_eq!(buffer.commits(), count);
        }
        let syncs = shard.memory.syncs();
        buffer.committed().await.expect("commits nothing");
        assert_eq!(buffer.commits(), 3, "a commit with nothing to write ended");
        assert_eq!(shard.memory.syncs(), syncs, "it synced nothing");
        shard.memory.fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 6, 1, None, Parts::default())])
            .expect("queues");
        let failed = Err(Error::Files(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        }));
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(buffer.commits(), 3, "the failed commit did not count");
    });
}

/// A move of `commits` does not make every entry durable: an entry appended while
/// a commit runs goes in the next one, and so does a `committed` future made then.
#[test]
fn commits_moves_before_an_entry_appended_during_the_commit_is_durable() {
    run(30, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        let tenths = |count: i64| Span::from_nanos(COMMIT.nanos() / 10 * count);
        shard.memory.slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(buffer.commits(), 0, "the first sync runs");
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues during the sync");
        let mut commit = pin!(buffer.committed());
        let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the future waits for the next commit");
        shard.clock.sleep(tenths(4)).await;
        assert_eq!(buffer.commits(), 1);
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
        let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the future still waits");
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
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(shard.memory.syncs(), 2);
        buffer
            .append([entry(1, a, Path::Live, 3, 1, None, Parts::default())])
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
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
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
            .append([entry(1, a, Path::Live, 0, 3, None, Parts::default())])
            .expect("queues");
        drop(buffer.append([entry(1, a, Path::Live, 2, 1, None, Parts::default())]));
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
fn an_entry_past_the_last_seq_is_a_broken_invariant() {
    let (mut sim, _handle) = start(11, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        drop(buffer.append([entry(
            1,
            a,
            Path::Live,
            u64::MAX,
            1,
            None,
            Parts::default(),
        )]));
    });
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: DIR.into(),
            message: "invariant: an entry of index \
                      00000000-0000-0000-0000-000000000001 on path Live starts at \
                      18446744073709551615 with 1 samples, past the last seq"
                .into(),
            seed: 11,
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
fn a_file_of_only_the_header_blocks_is_read_for_its_length() {
    run(107, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let blocks = shard.memory.bytes(RING)[..to_usize(AREA_START)].to_vec();
        let files = shard.memory.files();
        files.remove(FilePath::new(RING)).await.expect("removes");
        shard.zeroed(AREA_START).await;
        shard.memory.put(RING, 0, &blocks);
        let opened = shard
            .open(layout(2 * AREA, BODY_MAX), &mut Slots::new())
            .await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Length {
                expected: AREA_START + AREA,
                found: AREA_START,
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

/// Bytes past the first sector of a header block still make the file not a ring.
#[test]
fn a_file_with_bytes_past_the_first_sector_of_a_header_block_is_missing() {
    run(11, Memory::default(), |shard| async move {
        shard.zeroed(AREA_START + AREA).await;
        shard.memory.put(RING, COVER, b"not a ring");
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Missing));
    });
}

/// A checkpoint is in the first sector of its block, so a crash leaves each block
/// whole or zero. One whole block opens the ring with what it holds, and the open
/// leaves that block as it is.
#[test]
fn a_ring_with_one_zero_header_block_opens_from_the_other() {
    let block = to_usize(BLOCK);
    for (lost, kept) in [(0, block), (block, 0)] {
        run(102, Memory::default(), move |shard| async move {
            let ring = layout(AREA, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.assign(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            shard.memory.put(RING, lost, &[0; SECTOR]);
            let before = shard.memory.bytes(RING);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.assign(key(1));
            assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)), "{lost}");
            let after = shard.memory.bytes(RING);
            assert_eq!(after[kept..kept + block], before[kept..kept + block]);
        });
    }
}

/// The first write of the header goes to both blocks in one write. A crash that
/// keeps neither first sector leaves two zero blocks, and the ring opens as new:
/// both blocks get the same first checkpoint.
#[test]
fn a_ring_whose_first_header_write_was_lost_opens_as_new() {
    run(102, Memory::default(), |shard| async move {
        let block = to_usize(BLOCK);
        let ring = layout(AREA, BODY_MAX);
        drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
        for place in [0, block] {
            shard.memory.put(RING, place, &[0; SECTOR]);
        }
        let opened = shard.open(ring, &mut Slots::new()).await;
        assert_eq!(opened.map(|buffer| buffer.layout()), Ok(ring));
        let bytes = shard.memory.bytes(RING);
        assert_eq!(&bytes[..8], b"FNDNRING");
        assert_eq!(bytes[..block], bytes[block..2 * block]);
    });
}

/// A sim with `seed` and one node on it.
fn one_node(seed: u64) -> (sim::Sim, sim::node::Node) {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    (sim, node)
}

/// Starts a shard named `name` on `node` to run `main`.
fn on_node<F>(
    node: &sim::node::Node,
    name: &str,
    main: impl FnOnce(Tasks) -> F + Send + 'static,
) -> env::thread::Handle
where
    F: Future<Output = ()> + 'static,
{
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    node.shards().start(config, main).expect("the shard starts")
}

/// Runs `main` on a shard named `name` of `node` until it returns.
fn run_on<F>(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    name: &str,
    main: impl FnOnce(Tasks) -> F + Send + 'static,
) where
    F: Future<Output = ()> + 'static,
{
    let handle = on_node(node, name, main);
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}

/// A buffer config for the ring in `dir` on the files of `node`.
fn node_config(node: &sim::node::Node, tasks: Tasks, dir: &str) -> Config {
    let config = block::Config { budget: POOL };
    let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
    Config {
        files: node.files(),
        dir: PathBuf::from(dir),
        pool: Rc::new(pool),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        layout: layout(AREA, BODY_MAX),
        commit: COMMIT,
    }
}

/// Calls `at` with each seed of `seeds` and each cut from 0 ns in steps of `step`
/// ns, until `at` returns that the first open had ended at the cut.
fn each_cut(seeds: Range<u64>, step: i64, mut at: impl FnMut(u64, i64) -> bool) {
    for seed in seeds {
        let mut cut = 0;
        while !at(seed, cut) {
            cut += step;
        }
    }
}

/// Starts an open of the ring on `node`, and stops the node with `crash` `cut`
/// nanoseconds into it. Returns whether the open had ended.
fn cut_an_open(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    cut: i64,
    crash: sim::Crash,
) -> bool {
    let ended = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ended);
    let own = node.clone();
    drop(on_node(node, "cut", move |tasks| async move {
        let config = node_config(&own, tasks, DIR);
        let _buffer = Buffer::open(config, &mut Slots::new())
            .await
            .expect("the open ends well");
        flag.store(true, Ordering::Relaxed);
        std::future::pending::<()>().await;
    }));
    sim.run_for(Span::from_nanos(cut)).expect("the run goes on");
    sim.crash(node, crash);
    ended.load(Ordering::Relaxed)
}

/// Starts the first open of a ring on a new node with `seed`, and stops the node
/// with `crash` `cut` nanoseconds into it. Returns the sim, the node, and whether
/// the open had ended.
fn cut_the_first_open(
    seed: u64,
    cut: i64,
    crash: sim::Crash,
) -> (sim::Sim, sim::node::Node, bool) {
    let (mut sim, node) = one_node(seed);
    let ended = cut_an_open(&mut sim, &node, cut, crash);
    (sim, node, ended)
}

/// Opens the ring in `dir` on `node`, commits one entry, cuts the power, and opens
/// the ring again. Returns the tail that the last open recovers.
fn commit_cut_and_recover(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    dir: &'static str,
) -> Tail {
    let own = node.clone();
    run_on(sim, node, "commit", move |tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&own, tasks, dir);
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
    });
    sim.crash(node, sim::Crash::Power);
    let recovered = Arc::new(Mutex::new(None));
    let out = Arc::clone(&recovered);
    let own = node.clone();
    run_on(sim, node, "recover", move |tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&own, tasks, dir);
        let buffer = Buffer::open(config, &mut slots).await.expect("opens again");
        let a = slots.assign(key(1));
        *out.lock().expect("no panic held the lock") = Some(buffer.tail(a, Path::Live));
    });
    let recovered = recovered.lock().expect("no panic held the lock").take();
    recovered.expect("the last open ended")
}

/// A power cut at any point of the first open leaves a ring that opens again
/// with its layout. A cut while the first header write is in flight keeps any
/// set of its sectors.
#[test]
fn a_power_cut_during_the_first_open_leaves_a_ring_that_opens() {
    each_cut(0..64, 10_000, |seed, cut| {
        let (mut sim, node, ended) = cut_the_first_open(seed, cut, sim::Crash::Power);
        let opened = Arc::new(Mutex::new(None));
        let out = Arc::clone(&opened);
        let own = node.clone();
        run_on(&mut sim, &node, "second", move |tasks| async move {
            let config = node_config(&own, tasks, DIR);
            let buffer = Buffer::open(config, &mut Slots::new()).await;
            let layout = buffer.map(|buffer| buffer.layout());
            *out.lock().expect("no panic held the lock") = Some(layout);
        });
        let opened = opened.lock().expect("no panic held the lock").take();
        let opened = opened.expect("the second open ended");
        assert_eq!(
            opened,
            Ok(layout(AREA, BODY_MAX)),
            "seed {seed}, cut at {cut} ns"
        );
        ended
    });
}

#[test]
fn a_power_cut_after_the_first_commit_keeps_the_committed_entries() {
    let (mut sim, node) = one_node(1);
    let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
    assert_eq!(recovered, tail(3, Some(30)));
}

/// A kill can leave a ring that the first open made but did not make durable. The
/// next open finds it and makes it durable.
#[test]
fn a_kill_during_the_first_open_keeps_the_commits_of_the_next() {
    each_cut(0..16, 2_000, |seed, cut| {
        let (mut sim, node, ended) = cut_the_first_open(seed, cut, sim::Crash::Process);
        let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
        assert_eq!(
            recovered,
            tail(3, Some(30)),
            "seed {seed}, kill at {cut} ns"
        );
        ended
    });
}

/// A power cut at any point of an open whose restart record goes over the one of
/// an open with no data keeps the committed entry. The ring then takes the next
/// entry, and it survives a power cut.
#[test]
fn a_power_cut_during_a_restart_over_an_old_one_keeps_the_entries() {
    each_cut(0..32, 10_000, |seed, cut| {
        let (mut sim, node) = one_node(seed);
        sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks.clone(), DIR);
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            let a = slots.assign(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let config = node_config(&node, tasks, DIR);
            drop(
                Buffer::open(config, &mut Slots::new())
                    .await
                    .expect("reopens"),
            );
        })
        .expect("the run ends");
        let ended = cut_an_open(&mut sim, &node, cut, sim::Crash::Power);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks, DIR);
            let buffer = Buffer::open(config, &mut slots).await?;
            let a = slots.assign(key(1));
            let recovered = buffer.tail(a, Path::Live);
            buffer.append([entry(
                1,
                a,
                Path::Live,
                3,
                2,
                Some(50),
                Parts::default(),
            )])?;
            buffer.committed().await?;
            Ok::<_, Error>(recovered)
        });
        let recovered = recovered.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        assert_eq!(
            recovered,
            Ok(tail(3, Some(30))),
            "seed {seed}, cut at {cut} ns"
        );
        sim.crash(&node, sim::Crash::Power);
        let last = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks, DIR);
            let buffer = Buffer::open(config, &mut slots).await;
            buffer.map(|buffer| buffer.tail(slots.assign(key(1)), Path::Live))
        });
        let last = last.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        assert_eq!(last, Ok(tail(5, Some(50))), "seed {seed}, cut at {cut} ns");
        ended
    });
}

/// A failed sync of the ring's directory or of its parent fails the open. The next
/// open makes the ring durable.
#[test]
fn a_failed_directory_sync_fails_the_open_and_the_next_one_keeps_its_commits() {
    for dir in ["", DIR] {
        let (mut sim, node) = one_node(1);
        node.fail_file(FilePath::new(dir), Operation::SyncDir);
        let own = node.clone();
        run_on(&mut sim, &node, "first", move |tasks| async move {
            let config = node_config(&own, tasks, DIR);
            let opened = Buffer::open(config, &mut Slots::new()).await;
            let error = FileError::Io {
                path: PathBuf::from(dir),
                operation: Operation::SyncDir,
                code: 5,
            };
            assert_eq!(opened.map(drop), Err(Error::Files(error)));
        });
        let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
        assert_eq!(recovered, tail(3, Some(30)), "{dir:?}");
    }
}

/// The open syncs the parent of a nested ring directory, not the data directory.
#[test]
fn a_ring_in_a_nested_directory_keeps_its_commits_across_a_power_cut() {
    let (mut sim, node) = one_node(1);
    let own = node.clone();
    run_on(&mut sim, &node, "parent", move |_| async move {
        let files = own.files();
        files
            .create_dir(FilePath::new("a"))
            .await
            .expect("the dir is made");
        files
            .sync_dir(FilePath::new(""))
            .await
            .expect("the dir is durable");
    });
    let recovered = commit_cut_and_recover(&mut sim, &node, "a/shard-0");
    assert_eq!(recovered, tail(3, Some(30)));
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

/// A header whose `body_max` is under one block less the record header makes no
/// ring.
#[test]
fn a_header_with_a_body_under_one_block_is_unfit() {
    run(28, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let body_max = BODY_MAX - 1;
        let small = u32::try_from(body_max).expect("a small size");
        shard.tamper(BODY_MAX_AT, &small.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        let unfit = Unfit {
            area: AREA,
            body_max,
        };
        assert_eq!(opened.map(drop), Err(Error::Unfit(unfit)));
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
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        shard.tamper_record(BLOCK, 4 + 16, &[2]);
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Invalid { offset: BLOCK }));
    });
}

/// A whole record of 1023 entries opens, and one of 1024 is a wrong shape.
#[test]
fn a_record_over_the_most_entries_is_invalid() {
    for (count, opens) in [(1023_u32, true), (1024, false)] {
        run(102, Memory::default(), move |shard| async move {
            let ring = layout(64 * BLOCK, 100_000);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.assign(key(1));
            let len = 4 + 51 * to_usize(count.into());
            let parts = Parts::from(shard.block(len - 55));
            buffer
                .append([entry(1, a, Path::Live, 0, 1, Some(1), parts)])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let mut body = count.to_le_bytes().to_vec();
            for first in 0..u64::from(count) {
                let last = i64::try_from(first + 1).expect("fits");
                body.extend_from_slice(&1_u128.to_le_bytes());
                body.push(0);
                body.extend_from_slice(&first.to_le_bytes());
                body.extend_from_slice(&1_u32.to_le_bytes());
                body.extend_from_slice(&7_i64.to_le_bytes());
                body.push(1);
                body.extend_from_slice(&last.to_le_bytes());
                body.push(0);
                body.extend_from_slice(&0_u32.to_le_bytes());
            }
            assert_eq!(body.len(), len);
            shard.tamper_record(BLOCK, 0, &body);
            let mut slots = Slots::new();
            let opened = shard.open(ring, &mut slots).await;
            let tails =
                opened.map(|buffer| buffer.tail(slots.assign(key(1)), Path::Live));
            let expected = if opens {
                Ok(tail(count.into(), Some(count.into())))
            } else {
                Err(Error::Invalid { offset: BLOCK })
            };
            assert_eq!(tails, expected, "{count} entries");
        });
    }
}

#[test]
fn a_record_with_an_entry_past_the_last_seq_is_invalid() {
    run(103, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        // `first` of the first entry: after the count, the index, and the path.
        shard.tamper_record(BLOCK, 4 + 16 + 1, &u64::MAX.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Invalid { offset: BLOCK }));
    });
}

#[test]
fn a_record_with_an_entry_below_the_tail_is_invalid() {
    run(106, Memory::default(), |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.assign(key(1));
        buffer
            .append([
                entry(1, a, Path::Live, 0, 3, Some(30), Parts::default()),
                entry(1, a, Path::Live, 3, 2, Some(50), Parts::default()),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        // `first` of the second entry: after the count, one table of 51 bytes,
        // the index, and the path.
        shard.tamper_record(BLOCK, 4 + 51 + 16 + 1, &1u64.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Invalid { offset: BLOCK }));
    });
}

#[test]
fn a_header_with_an_area_at_the_end_of_u64_is_not_read() {
    run(104, Memory::default(), |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let area = u64::MAX - 4095;
        // The area is 8 bytes at offset 10 of a header block.
        shard.tamper(10, &area.to_le_bytes());
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Unfit(Unfit {
                area,
                body_max: BODY_MAX
            }))
        );
    });
}

#[test]
fn a_layout_with_an_area_at_the_end_of_u64_makes_no_ring() {
    let area = u64::MAX - 4095;
    assert_eq!(
        Layout::new(area, BODY_MAX),
        Err(Unfit {
            area,
            body_max: BODY_MAX
        })
    );
}

/// Opens with no data write their restart records at the same place, each with
/// a new chain.
#[test]
fn each_open_starts_a_new_chain() {
    run(20, Memory::default(), |shard| async move {
        let mut chains = Vec::new();
        for _ in 0..2 {
            let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
            drop(buffer.expect("opens"));
            let file = shard.memory.bytes(RING);
            let at = to_usize(AREA_START) + 9;
            let chain = file[at..at + 4].try_into().expect("four bytes");
            chains.push(u32::from_le_bytes(chain));
        }
        assert_ne!(chains[0], chains[1], "both opens drew the same chain");
    });
}

/// Opens with no data leave one restart record, so a ring opened more times than
/// it has blocks opens and takes its largest record.
#[test]
fn opens_with_no_data_leave_room_for_the_largest_record() {
    for area in [2 * BLOCK, AREA] {
        run(24, Memory::default(), move |shard| async move {
            let ring = layout(area, BODY_MAX);
            for _ in 0..area / BLOCK {
                drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.assign(key(1));
            let largest = Parts::from(shard.block(BODY_MAX - 55));
            let batch = [entry(1, a, Path::Live, 0, 1, None, largest)];
            assert_eq!(buffer.append(batch), Ok(()), "an area of {area}");
        });
    }
}

/// Each open with no data writes its restart record right after the last entry,
/// over the one before it. The entry survives, also when the first sector of the
/// last restart record holds other bytes, and the ring takes the next entry.
#[test]
fn opens_with_no_data_after_an_entry_leave_room_for_the_next() {
    for torn in [false, true] {
        run(25, Memory::default(), move |shard| async move {
            let ring = layout(6 * BLOCK, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.assign(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            for _ in 0..3 {
                drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
            }
            if torn {
                let after = to_usize(AREA_START + 2 * BLOCK);
                shard.memory.put(RING, after, &[0xA5; SECTOR]);
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.assign(key(1));
            assert_eq!(
                buffer.tail(a, Path::Live),
                tail(3, Some(30)),
                "torn: {torn}"
            );
            let next = [entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())];
            assert_eq!(buffer.append(next), Ok(()), "torn: {torn}");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("reopens");
            let a = slots.assign(key(1));
            assert_eq!(
                buffer.tail(a, Path::Live),
                tail(5, Some(50)),
                "torn: {torn}"
            );
        });
    }
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
        let parts = Parts::from(shard.block(3900));
        for seq in 0..2 {
            buffer
                .append([entry(1, a, Path::Live, seq, 1, None, parts.clone())])
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
                let parts = Parts::from(shard.block(bytes));
                buffer
                    .append([entry(index, slot, path, first, len, last, parts.clone())])
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
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
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
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
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

#[test]
fn a_record_over_the_largest_block_of_the_pool_is_recovered() {
    run(101, Memory::default(), |mut shard| async move {
        let parts_pool = Rc::clone(&shard.pool);
        let config = block::Config { budget: 96 << 10 };
        shard.pool =
            Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        assert_eq!(shard.pool.largest(), 80 << 10);
        let ring = layout(64 * BLOCK, 100_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.assign(key(1));
        let mut part = parts_pool.alloc(30_000).expect("the pool has a block");
        part.fill(7);
        let parts = Parts::from(part.freeze());
        buffer
            .append([
                entry(1, a, Path::Live, 0, 1, Some(1), parts.clone()),
                entry(1, a, Path::Live, 1, 1, Some(2), parts.clone()),
                entry(1, a, Path::Live, 2, 1, Some(3), parts),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(3)));
        drop(buffer);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.assign(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(3, Some(3))));
    });
}

/// A batch of one entry with `entry_max` bytes of parts commits after an entry that
/// opened a group, and is recovered at the next open.
#[test]
fn one_entry_of_the_entry_max_commits_and_is_recovered() {
    run(110, Memory::default(), |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.assign(key(1));
        let small = Parts::from(shard.block(10));
        let max = Parts::from(shard.block(buffer.layout().entry_max()));
        assert_eq!(buffer.layout().entry_max(), 4032);
        let appended = buffer.append([entry(1, a, Path::Live, 0, 1, None, small)]);
        assert_eq!(appended, Ok(()));
        let appended = buffer.append([entry(1, a, Path::Live, 1, 1, Some(1), max)]);
        assert_eq!(appended, Ok(()));
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.assign(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(2, Some(1))));
    });
}
