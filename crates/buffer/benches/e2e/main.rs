//! Scratch end-to-end timing of commits and reads through the public surface of
//! `buffer`, over the memory file driver of its tests. For the review of PR 1554.
#![allow(warnings, clippy::all, clippy::pedantic)]

#[path = "../../tests/it/memory.rs"]
mod memory;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use block::{Heap, Pool};
use buffer::{Buffer, Config, Entry, Layout, Mark, Parts};
use types::channel::{self, Slots};
use types::frame::Path;
use types::time::{Span, Stamp};

use crate::memory::Memory;

const COMMIT: Span = Span::from_nanos(10_000_000);
const CHANNELS: u32 = 64;
const RECORDS: u64 = 1024;
const SAMPLES: usize = 15;

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

fn main() {
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
            let config = block::Config { budget: 1 << 26 };
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
                layout: Layout::new(8192 * 4096, 8183).expect("the sizes make a ring"),
                commit: COMMIT,
            };
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            for index in 0..CHANNELS {
                slots.assign(channel::Key::from_u128(u128::from(index)));
            }
            let parts = Parts::from(pool.alloc(16).expect("a block").freeze());
            let started = Instant::now();
            for commit in 0..RECORDS {
                let batch: Vec<Entry> = (0..CHANNELS)
                    .map(|index| entry(index, commit, parts.clone()))
                    .collect();
                buffer.append(batch).expect("the ring has room");
                buffer.committed().await.expect("commits");
            }
            let commits = started.elapsed().as_nanos() as f64;
            println!(
                "commit: {:.1} ns per commit of {CHANNELS} entries, {:.2} ns per entry \
                 (one run of {RECORDS} commits, sim and memory driver included)",
                commits / RECORDS as f64,
                commits / (RECORDS as f64 * f64::from(CHANNELS)),
            );
            // Warm the pool classes and the caches.
            for index in 0..CHANNELS {
                let read = buffer
                    .read(channel::Slot::new(index), Path::Live, Mark::at(0), usize::MAX)
                    .await
                    .expect("reads");
                assert_eq!(read.entries.len() as u64, RECORDS);
            }
            let mut per_record = Vec::new();
            for _ in 0..SAMPLES {
                let started = Instant::now();
                for index in 0..CHANNELS {
                    let read = buffer
                        .read(
                            channel::Slot::new(index),
                            Path::Live,
                            Mark::at(0),
                            usize::MAX,
                        )
                        .await
                        .expect("reads");
                    std::hint::black_box(&read);
                }
                let nanos = started.elapsed().as_nanos() as f64;
                per_record.push(nanos / (f64::from(CHANNELS) * RECORDS as f64));
            }
            per_record.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            println!(
                "read_whole_path: ns per record visited: min {:.1}, median {:.1}, max \
                 {:.1} ({SAMPLES} samples of {CHANNELS} reads of {RECORDS} records)",
                per_record[0],
                per_record[SAMPLES / 2],
                per_record[SAMPLES - 1],
            );
            // A read that finds nothing: one lookup and no file read.
            let mut per_read = Vec::new();
            for _ in 0..SAMPLES {
                let started = Instant::now();
                for _ in 0..1000 {
                    for index in 0..CHANNELS {
                        let read = buffer
                            .read(
                                channel::Slot::new(index),
                                Path::Live,
                                Mark::at(RECORDS),
                                usize::MAX,
                            )
                            .await
                            .expect("reads");
                        std::hint::black_box(&read);
                    }
                }
                let nanos = started.elapsed().as_nanos() as f64;
                per_read.push(nanos / (f64::from(CHANNELS) * 1000.0));
            }
            per_read.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            println!(
                "read_at_tail: ns per read that gives nothing: min {:.2}, median {:.2}, \
                 max {:.2} ({SAMPLES} samples of {} reads)",
                per_read[0],
                per_read[SAMPLES / 2],
                per_read[SAMPLES - 1],
                u64::from(CHANNELS) * 1000,
            );
            // Control: no code of `buffer` runs. One pool block taken and given back.
            let mut per_block = Vec::new();
            for _ in 0..SAMPLES {
                let started = Instant::now();
                for _ in 0..64_000 {
                    let block = pool.alloc(std::hint::black_box(16)).expect("a block");
                    std::hint::black_box(&block);
                    drop(block);
                }
                per_block.push(started.elapsed().as_nanos() as f64 / 64_000.0);
            }
            per_block.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            println!(
                "control_pool: ns per block taken and given back: min {:.3}, median \
                 {:.3}, max {:.3} ({SAMPLES} samples of 64000 blocks)",
                per_block[0],
                per_block[SAMPLES / 2],
                per_block[SAMPLES - 1],
            );
            drop(buffer);
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}
