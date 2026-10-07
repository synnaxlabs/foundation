//! MEASUREMENT ONLY (#1149): one simulated minute of the edge buffer at 1M samples/s
//! on the sim disk. Args: frame period in ms, bytes a sample, seconds.

#![expect(clippy::disallowed_macros, reason = "measurement")]
#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use block::{Heap, Pool};
use buffer::{Buffer, Config, Entry, Layout, Parts};
use types::channel::{self, Slots};
use types::frame::Path;
use types::time::{Span, Stamp};

const RATE: u64 = 1_000_000;

fn hwm_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().unwrap()).collect();
    let (period_ms, bytes, seconds) = (args[0], args[1], args[2]);
    let samples = RATE * period_ms / 1000;
    let frames = seconds * 1000 / period_ms;
    let entry_bytes = (samples * bytes) as usize;
    let body_max = (entry_bytes + 8192).next_multiple_of(4096).max(8183);
    let area = (RATE * bytes * seconds * 11 / 10).next_multiple_of(4096) + (64 << 20);
    let start_hwm = hwm_kib();
    let wall = Instant::now();
    let mut sim = sim::Sim::new(sim::Config {
        steps_max: u64::MAX,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let entropy = node.entropy();
    let host = node.clone();
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: None,
    };
    let commits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let out = std::sync::Arc::clone(&commits);
    let handle = node
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config { budget: 1 << 28 };
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let mut slots = Slots::new();
            let files = host.files();
            files.create_dir(std::path::Path::new("shard-0")).await.unwrap();
            let config = Config {
                files,
                dir: PathBuf::from("shard-0"),
                pool: Rc::clone(&pool),
                clock: clock.clone(),
                tasks,
                entropy,
                layout: Layout::new(area, body_max).unwrap(),
                commit: Span::from_nanos(10_000_000),
            };
            let buffer = Buffer::open(config, &mut slots).await.unwrap();
            let key = channel::Key::from_u128(1);
            let slot = slots.assign(key);
            let parts = Parts::from(pool.alloc(entry_bytes).unwrap().freeze());
            let period = Span::from_nanos(period_ms as i64 * 1_000_000);
            let mut next = clock.now();
            for frame in 0..frames {
                buffer
                    .append([Entry {
                        index: key,
                        slot,
                        path: Path::Live,
                        first: frame * samples,
                        len: samples as u32,
                        stored_at: Stamp::from_nanos(7),
                        last: Some(Stamp::from_nanos(7)),
                        tag: 0,
                        parts: parts.clone(),
                    }])
                    .unwrap();
                next = next + period;
                clock.sleep_until(next).await;
            }
            buffer.committed().await.unwrap();
            out.store(buffer.commits(), std::sync::atomic::Ordering::Relaxed);
        })
        .unwrap();
    sim.run().unwrap();
    handle.join().unwrap();
    let commits = commits.load(std::sync::atomic::Ordering::Relaxed);
    let wall = wall.elapsed();
    let steps = sim.steps();
    let hwm = hwm_kib();
    println!(
        "period_ms={period_ms} bytes={bytes} seconds={seconds} frames={frames} \
         commits={commits} steps={steps} wall_s={:.2} hwm_mib={} start_mib={}",
        wall.as_secs_f64(),
        hwm / 1024,
        start_hwm / 1024
    );
}
