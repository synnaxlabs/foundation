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

#[path = "../tests/common/mod.rs"]
#[expect(dead_code, reason = "this bench has no hub")]
mod common;
mod table;

use std::sync::Arc;

use common::name;
use home::Outcome;
use home::reader::complete::Charge;
use table::{Line, clocked};
use types::authority::Authority;
use types::channel;
use types::frame::key_set::Group;
use types::frame::{Draft, Form, Label, Path};
use types::sample::{Scalar, Type};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The counts of indexes, each with the name of its line.
const INDEXES: [(usize, &str); 3] =
    [(1, "woken 1"), (16, "woken 16"), (256, "woken 256")];
/// `WARMUP + ROUNDS` commits, under the 113 of 256 frames that the ring of
/// `common::shard` holds.
const WARMUP: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let mut lines = Vec::new();
    for (indexes, line) in INDEXES {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let run = move |node, tasks| bench(node, tasks, indexes, line);
        let (timer, woken) = sim.run_on(&node, run).expect("the run ends");
        if lines.is_empty() {
            lines.push(timer);
        }
        lines.push(woken);
    }
    table::print(&format!("ns per call over {ROUNDS} rounds"), &lines);
}

/// The `timer` line and the line named `line` of a shard with `indexes` indexes.
async fn bench(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    indexes: usize,
    line: &'static str,
) -> (Line, Line) {
    let (mut shard, mut interner, mut stamp) = common::shard(&node, tasks).await;
    let channels: Vec<_> = (0..indexes)
        .map(|n| {
            let key = |k| channel::Key::from_u128(u128::try_from(k).expect("few"));
            (
                key(2 * n + 1),
                [(key(2 * n + 2), Type::Scalar(Scalar::I64))],
            )
        })
        .collect();
    let groups: Vec<_> = channels
        .iter()
        .map(|(index, data)| Group {
            index: *index,
            data,
        })
        .collect();
    let set = interner.intern(&groups);
    let mut readers = Vec::with_capacity(indexes);
    for &entry in set.groups() {
        let slot = set.entries()[entry].slot;
        shard.carry(slot);
        readers.push(shard.open_complete(slot, u64::MAX, Charge::Whole).into());
    }
    let writer = shard
        .open_writer(home::writer::Writer {
            subject: name("bench"),
            authority: Authority(1),
            lease: None,
            set: Arc::clone(&set),
        })
        .expect("opens");
    let series: Vec<_> = (0..set.entries().len()).map(|entry| (entry, 8)).collect();
    let mut keys = Vec::new();
    let (mut timer, mut woken) = (Line::new("timer", 1), Line::new(line, 1));
    for round in 0..WARMUP + ROUNDS {
        let mut draft =
            Draft::new(shard.pool(), &set, Form::Raw, &series).expect("a frame");
        for &(entry, _) in &series {
            let bytes = draft.series_mut(entry).expect("the series is present");
            bytes.copy_from_slice(&stamp.to_le_bytes());
        }
        for group in 0..set.groups().len() {
            draft.set_count(u32::try_from(group).expect("few"), 1);
        }
        stamp += 1;
        let written = shard.write(writer, Label::Path(Path::Live), draft);
        let applied = written.is_ok_and(|outcomes| {
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Outcome::Applied { .. }))
        });
        assert!(applied, "the home applies round {round}");
        shard.woken(&mut keys);
        assert!(keys.is_empty(), "a write wakes no complete reader");
        shard.committed().await.expect("commits");
        timer.add(timed(|| ()));
        woken.add(timed(|| shard.woken(&mut keys)));
        assert_eq!(keys.len(), indexes, "the commit wakes each reader");
        for &reader in &readers {
            assert!(shard.take(reader).is_some(), "a frame waits for the reader");
        }
        timer.close(round >= WARMUP);
        woken.close(round >= WARMUP);
    }
    (timer, woken)
}

/// The ns that `f` takes, and the allocations it makes.
fn timed(f: impl FnOnce()) -> (u64, u64) {
    let (((), span), counted) = ALLOCATOR.count(|| clocked(f));
    (span, counted)
}
