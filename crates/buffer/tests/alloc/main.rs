//! `append` makes no heap allocation once the buffer has taken a batch of each
//! shape: its slots, its entry count, and whether it closes the open record. Nor
//! does a `Commit` that waits and drops. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../it/memory.rs"]
#[expect(dead_code, reason = "this binary uses only the driver")]
mod memory;

use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::{Heap, Pool};
use buffer::{Buffer, Config, Entry, Layout, Mark, Parts, Read};
use types::channel::{self, Slots};
use types::frame::Path;
use types::time::{Span, Stamp};

use crate::memory::Memory;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const COMMIT: Span = Span::from_nanos(10_000_000);
/// Commits that warm the buffer up: its vectors reach their size and its groups
/// cycle through the spares.
const WARM: u64 = 3;
/// Commits whose `append` is counted.
const COUNTED: u64 = 8;
/// A record body that holds the wide batch, and no more than two of the batches
/// that close a record.
const BODY_MAX: usize = 8183;
/// Indexes of the wide batch.
const WIDE: u32 = 64;

fn entry(index: u32, first: u64, parts: Parts) -> Entry {
    Entry {
        index: channel::Key::from_u128(u128::from(index)),
        slot: channel::Slot::new(index),
        path: Path::Live,
        first,
        len: 1,
        stored_at: Stamp::from_nanos(7),
        last: Some(Stamp::from_nanos(7)),
        tag: 0,
        parts,
    }
}

/// The six batch shapes of one commit: empty, two indexes, empty, every other
/// index, one half-body entry, and a half-body entry with a small one.
fn batches(commit: u64, parts: &Parts, half: &Parts) -> [Vec<Entry>; 6] {
    let seq = 3 * commit;
    let wide: Vec<Entry> = (2..WIDE)
        .map(|index| entry(index, commit, Parts::default()))
        .collect();
    [
        Vec::new(),
        vec![entry(0, seq, parts.clone()), entry(1, seq, parts.clone())],
        Vec::new(),
        wide,
        vec![entry(0, seq + 1, half.clone())],
        vec![
            entry(0, seq + 2, half.clone()),
            entry(1, seq + 1, parts.clone()),
        ],
    ]
}

/// Reads the whole log of `slot` in one poll, with the allocations the read
/// made.
fn read_once(buffer: &Buffer, slot: channel::Slot) -> (Read, u64) {
    let (read, allocations) = ALLOCATOR.count(|| {
        let mut read = pin!(buffer.read(slot, Path::Live, Mark::at(0), usize::MAX));
        match read.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(read) => read,
            Poll::Pending => panic!("a read of the memory driver is ready at once"),
        }
    });
    (read.expect("reads"), allocations)
}

/// Polls a `Commit` of queued entries once and drops it. From commit `WARM` on,
/// checks that it made no allocation.
fn wait_once(buffer: &Buffer, commit: u64) {
    let (polled, allocations) = ALLOCATOR.count(|| {
        let waiter = pin!(buffer.committed());
        waiter.poll(&mut Context::from_waker(Waker::noop()))
    });
    assert!(polled.is_pending(), "a commit of queued entries waits");
    if commit >= WARM {
        assert_eq!(allocations, 0, "a wait allocated at commit {commit}");
    }
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let entropy = node.entropy();
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: None,
    };
    let handle = node
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config::new(1 << 21).expect("the budget fits");
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let mut slots = Slots::new();
            let config = Config {
                files: Memory::default().files(),
                dir: PathBuf::from("shard-0"),
                pool: Rc::clone(&pool),
                clock,
                tasks,
                entropy,
                layout: Layout::new(256 * 4096, BODY_MAX)
                    .expect("the sizes make a ring"),
                commit: COMMIT,
            };
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            for index in 0..WIDE {
                slots.index(channel::Key::from_u128(u128::from(index)));
            }
            let parts = Parts::from(pool.alloc(256).expect("a block").freeze());
            let half = Parts::from(pool.alloc(BODY_MAX / 2).expect("a block").freeze());
            for commit in 0..WARM + COUNTED {
                let batches = batches(commit, &parts, &half);
                for (shape, batch) in batches.into_iter().enumerate() {
                    let (appended, allocations) =
                        ALLOCATOR.count(|| buffer.append(batch));
                    appended.expect("the ring has room");
                    if commit >= WARM {
                        assert_eq!(
                            allocations, 0,
                            "append allocated at commit {commit}, shape {shape}"
                        );
                    }
                }
                wait_once(&buffer, commit);
                buffer.committed().await.expect("commits");
            }
            let (read, allocations) = read_once(&buffer, channel::Slot::new(1));
            assert_eq!(
                read.entries.len(),
                2,
                "index 1 has two entries before its skip"
            );
            assert_eq!(
                allocations, 7,
                "the entries, the entries of a record to give, and one boxed file call \
                 per entry and per table: the two records with an entry, and the one \
                 where the skip ahead starts"
            );
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}
