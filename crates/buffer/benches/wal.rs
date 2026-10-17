//! The time for each commit of the write-ahead ring with no file, and its records per
//! second: `append` and `synced` for each record, `trimmed` and `release` for each
//! commit. A commit of 1 record shows the cost for each commit, one of 8 the cost for
//! each record. The ring of ID 64 wraps often, the one of ID 4096 seldom. Each time
//! also holds the bench's own work to make the ends of each record, so a change of the
//! writer shows smaller than it is. Each ring has [`PHASE`] blocks more than its
//! argument. A commit trims only once the tail is behind the low mark, so each sample
//! of each bench but `deadline_carry` and `deadline_trim` holds [`SAMPLE_SIZE`] commits
//! and at least 10 trims. `commit_group` times one group commit of 1 or 32 entries on
//! the ring of ID 64: the fit, which checks that the record fits, the take, the close,
//! the seal with the CRC of the body, and the same trim and sync. `queue_group` adds
//! the append of each entry to the logs, as the buffer queues a batch. `deadline` times
//! the mean deadline of the commit task, which trims in steps, with an entry of each of
//! 1 or 32 indexes: the work of `queue_group` with the checkpoints of its trims and the
//! sync of the logs.
//! `deadline_carry` times one that also writes a carry record of 32 tails.
//! `deadline_trim` times only the deadline that trims a step of 100 blocks on the ring
//! of ID 4096.

use std::num::NonZeroU32;

use buffer::bench::{Deadline, Group, PHASE, Ring, SAMPLE_SIZE};
use divan::Bencher;
use divan::counter::ItemsCount;

fn main() {
    divan::main();
}

#[divan::bench(args = [64, 4096], sample_size = SAMPLE_SIZE)]
fn commit_1_record(bencher: Bencher<'_, '_>, blocks: u64) {
    cycle(bencher, blocks, 1);
}

#[divan::bench(args = [64, 4096], sample_size = SAMPLE_SIZE)]
fn commit_8_records(bencher: Bencher<'_, '_>, blocks: u64) {
    cycle(bencher, blocks, 8);
}

fn cycle(bencher: Bencher<'_, '_>, blocks: u64, records: usize) {
    let mut ring = steady_ring(blocks, records);
    bencher
        .counter(ItemsCount::new(records))
        .bench_local(|| ring.commit(divan::black_box(records)));
}

/// The ring of `blocks`, after the warm-up, so a time holds the steady ring, which
/// trims in steps, not one still filling. Out of line, so that a change of the
/// setup does not change the code of the timed loop.
#[inline(never)]
fn steady_ring(blocks: u64, records: usize) -> Ring {
    let mut ring = Ring::new(blocks + PHASE);
    for _ in 0..2 * blocks {
        ring.commit(records);
    }
    ring
}

#[divan::bench(args = [1, 32], sample_size = SAMPLE_SIZE)]
fn commit_group(bencher: Bencher<'_, '_>, entries: usize) {
    let mut group = steady_group(entries, Group::commit);
    bencher
        .counter(ItemsCount::new(entries))
        .bench_local(|| group.commit(divan::black_box(entries)));
}

#[divan::bench(args = [1, 32], sample_size = SAMPLE_SIZE)]
fn queue_group(bencher: Bencher<'_, '_>, entries: usize) {
    let mut group = steady_group(entries, Group::queue);
    bencher
        .counter(ItemsCount::new(entries))
        .bench_local(|| group.queue(divan::black_box(entries)));
}

#[divan::bench(args = [1, 32], sample_size = SAMPLE_SIZE)]
fn deadline(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut deadline = Deadline::new(indexes, 64 + PHASE);
    for _ in 0..2 * 64 {
        deadline.commit(1);
    }
    bencher
        .counter(ItemsCount::new(usize::try_from(indexes).expect("fits")))
        .bench_local(|| deadline.commit(divan::black_box(1)));
}

/// As [`deadline`], with each entry of tag 1, so each sync copies its 100 bytes into
/// the copy that it replaces.
#[divan::bench(args = [1, 32], sample_size = SAMPLE_SIZE)]
fn deadline_tagged(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut deadline = Deadline::tagged(indexes, 64 + PHASE);
    for _ in 0..2 * 64 {
        deadline.commit(1);
    }
    bencher
        .counter(ItemsCount::new(usize::try_from(indexes).expect("fits")))
        .bench_local(|| deadline.commit(divan::black_box(1)));
}

/// The last of 32 indexes appends one entry each 4096 deadlines, so the deadline
/// after 13 carries its tail and fills the record with the tails of the other 31.
#[divan::bench]
fn deadline_carry(bencher: Bencher<'_, '_>) {
    bencher
        .counter(ItemsCount::new(31_usize))
        .with_inputs(|| {
            let mut deadline = Deadline::new(32, 64 + PHASE);
            for _ in 0..13 {
                deadline.commit(4096);
            }
            deadline
        })
        .bench_local_values(|mut deadline| {
            assert_eq!(deadline.commit(4096), 32, "the deadline carries 32 tails");
            deadline
        });
}

/// The deadline of 1 or 32 indexes after [`Deadline::trimming`], which trims a step.
#[divan::bench(args = [1, 32], sample_size = 1)]
fn deadline_trim(bencher: Bencher<'_, '_>, indexes: u32) {
    bencher
        .counter(ItemsCount::new(usize::try_from(indexes).expect("fits")))
        .with_inputs(|| {
            Deadline::trimming(NonZeroU32::new(indexes).expect("each arg is nonzero"))
        })
        .bench_local_values(|mut deadline| {
            deadline.commit(divan::black_box(1));
            deadline
        });
}

/// The group ring of 64 blocks after the warm-up of `commit`, out of line as
/// [`steady_ring`] is.
#[inline(never)]
fn steady_group(entries: usize, commit: fn(&mut Group, usize)) -> Group {
    let mut group = Group::new(64 + PHASE);
    for _ in 0..2 * 64 {
        commit(&mut group, entries);
    }
    group
}
