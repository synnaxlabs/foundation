//! The time to sync the durable logs of a shard at each commit, for each entry: one
//! record with data entries of each index, after a trim that hides each older record.
//! Also the time to append an entry, to carry a tail, the time of a commit that
//! trims to the oldest holder, and the time of a commit that marks a range. Also
//! the time of a sync and a trim of entries with no samples, which mark ranges.

use buffer::bench::Logs;
use divan::Bencher;
use divan::counter::ItemsCount;

fn main() {
    divan::main();
}

/// Times logs that hold their runs, not logs still filling.
fn warm(indexes: u32) -> Logs {
    let mut logs = Logs::new(indexes);
    for _ in 0..16 {
        logs.commit(1);
    }
    logs
}

fn count(indexes: u32) -> ItemsCount {
    ItemsCount::new(usize::try_from(indexes).expect("u32 fits usize"))
}

/// Each entry starts a run in its record.
#[divan::bench(args = [1, 64, 256])]
fn sync(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = warm(indexes);
    bencher
        .counter(count(indexes))
        .bench_local(|| logs.commit(1));
}

/// Four entries of each index in each record, so three of four start no run.
#[divan::bench(args = [1, 64, 256])]
fn sync_four(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = warm(indexes);
    bencher
        .counter(count(indexes * 4))
        .bench_local(|| logs.commit(4));
}

#[divan::bench(args = [1, 64, 256])]
fn append(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = warm(indexes);
    bencher
        .counter(count(indexes))
        .bench_local(|| logs.append());
}

/// A commit with an append and a sync of each entry, then a carry record of the
/// tail of each index. Less `append` and `sync`, the time to carry each tail.
#[divan::bench(args = [1, 64, 256])]
fn carry(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = warm(indexes);
    bencher.counter(count(indexes)).bench_local(|| logs.carry());
}

/// One index syncs each 64th commit, so the records after its holder stay.
#[divan::bench(args = [64, 256])]
fn trim(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = trimmed(indexes, 0);
    bencher
        .counter(count(indexes))
        .bench_local(|| logs.trim(64, 0));
}

/// As `trim`, while a read holds the record 64 commits back, so no trim passes the
/// items before the oldest holder.
#[divan::bench(args = [64, 256])]
fn trim_held(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = trimmed(indexes, 64);
    bencher
        .counter(count(indexes))
        .bench_local(|| logs.trim(64, 64));
}

/// Each commit syncs an entry with no samples of each index, so each marks a range,
/// and carries each tail. The trim of the next commit passes the range. The oldest
/// holder is the carry record, past each range, so no landing moves.
#[divan::bench(args = [1, 64, 256])]
fn mark(bencher: Bencher<'_, '_>, indexes: u32) {
    let mut logs = Logs::new(indexes);
    for _ in 0..16 {
        logs.mark();
    }
    bencher.counter(count(indexes)).bench_local(|| logs.mark());
}

/// Each entry has no samples and comes at the seq of the entry before, in a new
/// record, so each sync marks a range that merges with the one before.
#[divan::bench(args = [1, 64, 256])]
fn sync_empty(bencher: Bencher<'_, '_>, indexes: u32) {
    bencher
        .counter(count(indexes))
        .with_inputs(|| empty(indexes))
        .bench_local_refs(|logs| logs.commit(1));
}

/// As `trim`, with the entries of `sync_empty`, so each landing searches the
/// ranges.
#[divan::bench(args = [64, 256])]
fn trim_marked(bencher: Bencher<'_, '_>, indexes: u32) {
    bencher
        .counter(count(indexes))
        .with_inputs(|| empty(indexes))
        .bench_local_refs(|logs| logs.trim(64, 0));
}

/// Logs of entries with no samples, with the runs of 12 records and room for one
/// more, so the timed commit takes no allocation. No holder moves, so each input is
/// new and the runs do not grow over the samples.
fn empty(indexes: u32) -> Logs {
    let mut logs = Logs::empty(indexes);
    for _ in 0..12 {
        logs.commit(1);
    }
    logs
}

fn trimmed(indexes: u32, held: u64) -> Logs {
    let mut logs = Logs::new(indexes);
    for _ in 0..128 {
        logs.trim(64, held);
    }
    logs
}
