//! Scratch benchmark of `log::Logs`, for the review of PR 1554. Not for the repo.
#![allow(warnings, clippy::all, clippy::pedantic)]

#[path = "../../src/entry.rs"]
mod entry;
#[path = "../../src/log.rs"]
mod log;
mod shim;

use divan::{Bencher, black_box};
use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;
use crate::log::{Logs, Mark, Tail};

fn main() {
    divan::main();
}

pub(crate) fn header(index: u32, first: u64, len: u32) -> Header {
    Header {
        index: channel::Key::from_u128(u128::from(index)),
        path: Path::Live,
        first,
        len,
        stored_at: Stamp::from_nanos(7),
        last: Some(Stamp::from_nanos(7)),
        tag: 0,
        bytes: 0,
    }
}

/// `paths` paths, each with one entry of one sample in each of `records` records.
/// Record `r` is at offset `4096 * (r + 1)`.
pub(crate) fn create_logs(paths: u32, records: u64) -> Logs {
    let mut logs = Logs::default();
    for record in 0..records {
        for index in 0..paths {
            let header = header(index, record, 1);
            logs.append(Slot::new(index), &header).expect("appends");
            logs.sync(Slot::new(index), &header, 4096 * (record + 1))
                .expect("syncs");
        }
    }
    logs
}

/// Control: the code of `Logs::append` did not change. One batch: one entry for
/// each path.
#[divan::bench(args = [1, 64, 1024])]
fn append_batch(bencher: Bencher<'_, '_>, paths: u32) {
    let mut logs = create_logs(paths, 12);
    let mut seq = 12;
    bencher.bench_local(|| {
        for index in 0..paths {
            let header = header(index, seq, 1);
            logs.append(Slot::new(index), black_box(&header))
                .expect("appends");
        }
        seq += 1;
    });
}

/// One more entry of each path in the newest record of the path: no new run.
#[divan::bench(args = [1, 64, 1024])]
fn sync_same_record(bencher: Bencher<'_, '_>, paths: u32) {
    let mut logs = create_logs(paths, 12);
    let mut seq = 12;
    bencher.bench_local(|| {
        for index in 0..paths {
            let header = header(index, seq, 1);
            logs.sync(Slot::new(index), black_box(&header), black_box(4096 * 12))
                .expect("syncs");
        }
        seq += 1;
    });
}

/// One entry of each path in a new record: one new run for each path, with room in
/// its deque.
#[divan::bench(args = [1, 64, 1024])]
fn sync_new_record(bencher: Bencher<'_, '_>, paths: u32) {
    bencher
        .with_inputs(|| create_logs(paths, 12))
        .bench_local_refs(|logs| {
            for index in 0..paths {
                let header = header(index, 12, 1);
                logs.sync(Slot::new(index), black_box(&header), black_box(4096 * 13))
                    .expect("syncs");
            }
        });
}

/// From empty: 256 records, one entry of each path in each. The deques grow.
#[divan::bench(args = [1, 64, 1024])]
fn sync_fill_256_records(bencher: Bencher<'_, '_>, paths: u32) {
    bencher.bench_local(|| {
        let mut logs = Logs::default();
        for record in 0..256_u64 {
            for index in 0..paths {
                let header = header(index, record, 1);
                logs.sync(Slot::new(index), black_box(&header), 4096 * (record + 1))
                    .expect("syncs");
            }
        }
        logs
    });
}

/// One lookup of a path with `runs` runs among 64 paths. The marks go up one
/// record at a time, as the walk of a read does.
#[divan::bench(args = [16, 4096, 262_144])]
fn lookup_in_order(bencher: Bencher<'_, '_>, runs: u64) {
    let mut logs = create_logs(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1))
            .expect("syncs");
    }
    let mut seq = 0;
    bencher.bench_local(|| {
        let found = shim::lookup(&logs, Slot::new(0), Path::Live, black_box(Mark::at(seq)));
        seq = if seq + 1 == runs { 0 } else { seq + 1 };
        found
    });
}

/// As `lookup_in_order`, with marks that jump over the runs.
#[divan::bench(args = [16, 4096, 262_144])]
fn lookup_scattered(bencher: Bencher<'_, '_>, runs: u64) {
    let mut logs = create_logs(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1))
            .expect("syncs");
    }
    let mut seq = 0_u64;
    bencher.bench_local(|| {
        let found = shim::lookup(&logs, Slot::new(0), Path::Live, black_box(Mark::at(seq)));
        seq = (seq.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407) >> 33) % runs;
        found
    });
}

/// A lookup of a path that the logs do not hold.
#[divan::bench]
fn lookup_unknown_path(bencher: Bencher<'_, '_>) {
    let logs = create_logs(64, 12);
    bencher.bench_local(|| {
        shim::lookup(&logs, black_box(Slot::new(99)), Path::Live, black_box(Mark::at(0)))
    });
}

/// `sync_same_record` with 100,000 paths of 64 runs each: 154 MB of runs, so the
/// front and the back of a deque are cold lines.
#[divan::bench(sample_count = 30, sample_size = 1)]
fn sync_same_record_wide(bencher: Bencher<'_, '_>) {
    const PATHS: u32 = 100_000;
    let mut logs = create_logs(PATHS, 64);
    let mut seq = 64;
    bencher.bench_local(|| {
        for index in 0..PATHS {
            let header = header(index, seq, 1);
            logs.sync(Slot::new(index), black_box(&header), black_box(4096 * 64))
                .expect("syncs");
        }
        seq += 1;
    });
}

/// `sync_new_record` with 100,000 paths of 12 runs each: one new run for each path,
/// with cold lines.
#[divan::bench(sample_count = 30, sample_size = 1)]
fn sync_new_record_wide(bencher: Bencher<'_, '_>) {
    const PATHS: u32 = 100_000;
    bencher
        .with_inputs(|| create_logs(PATHS, 12))
        .bench_local_refs(|logs| {
            for index in 0..PATHS {
                let header = header(index, 12, 1);
                logs.sync(Slot::new(index), black_box(&header), black_box(4096 * 13))
                    .expect("syncs");
            }
        });
}

/// Control for `sync_same_record_wide`: the code of `Logs::append` did not change.
#[divan::bench(sample_count = 30, sample_size = 1)]
fn append_batch_wide(bencher: Bencher<'_, '_>) {
    const PATHS: u32 = 100_000;
    let mut logs = create_logs(PATHS, 64);
    let mut seq = 64;
    bencher.bench_local(|| {
        for index in 0..PATHS {
            let header = header(index, seq, 1);
            logs.append(Slot::new(index), black_box(&header))
                .expect("appends");
        }
        seq += 1;
    });
}

/// Strict control: the code of `Tail::advance` is the same text in each tree, and
/// no changed code is under it.
#[divan::bench(args = [1, 64, 1024])]
fn control_tail_advance(bencher: Bencher<'_, '_>, paths: u32) {
    let mut tails = vec![Tail::default(); paths as usize];
    let mut seq = 0;
    bencher.bench_local(|| {
        for (index, tail) in tails.iter_mut().enumerate() {
            let header = header(index as u32, seq, 1);
            black_box(&mut *tail)
                .advance(black_box(&header))
                .expect("advances");
        }
        seq += 1;
    });
}
