//! The cost of `home::Shard::woken` after a commit, on one shard of a sim node with no
//! hub. Run with `cargo bench -p hub --bench woken`.
//!
//! For each count N of indexes, a shard carries N indexes, each with one data channel
//! and one complete reader. Each round writes one frame on every index, waits for its
//! commit, and times these lines:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `woken N`: the call of `woken` after the commit. It gives the N readers.
//!
//! The readers then take their frames, untimed. Judge a figure by `net`, its p50 less
//! the floor's.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../tests/common/shard.rs"]
mod shard;
mod table;
#[path = "../tests/common/woken.rs"]
mod woken;

use home::reader::Next;
use table::Line;
use woken::Woken;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The counts of indexes, each with the name of its line.
const INDEXES: [(usize, &str); 3] =
    [(1, "woken 1"), (16, "woken 16"), (256, "woken 256")];
/// `WARMUP + ROUNDS` commits, under the 113 of 256 frames that the ring of
/// `shard::shard` holds.
const WARMUP: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let (mut timer, mut lines) = (None, Vec::new());
    for (indexes, line) in INDEXES {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let run = move |node, tasks| bench(node, tasks, indexes, line);
        let (floor, woken) = sim.run_on(&node, run).expect("the run ends");
        timer.get_or_insert(floor);
        lines.push(woken);
    }
    let timer = timer.expect("a count of indexes");
    table::print(&format!("ns per call over {ROUNDS} rounds"), &timer, &lines);
}

/// The `timer` line and the line named `line` of a shard with `indexes` indexes.
async fn bench(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    indexes: usize,
    line: &'static str,
) -> (Line, Line) {
    let mut woken = Woken::new(&node, tasks, indexes).await;
    let mut readers = woken.readers.clone();
    readers.sort_unstable();
    let (mut timer, mut calls) = (Line::new("timer", 1), Line::new(line, 1));
    for round in 0..WARMUP + ROUNDS {
        woken.commit().await;
        timer.add(table::timed(&ALLOCATOR, || ()).1);
        let call = || woken.shard.woken(&mut woken.keys);
        calls.add(table::timed(&ALLOCATOR, call).1);
        assert_eq!(woken.keys, readers, "the commit wakes each reader");
        for &reader in &woken.readers {
            assert!(
                matches!(woken.shard.take(reader), Next::Frame(_)),
                "a frame waits for the reader"
            );
        }
        timer.close(round >= WARMUP);
        calls.close(round >= WARMUP);
    }
    (timer, calls)
}
