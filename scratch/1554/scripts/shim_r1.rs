use divan::{Bencher, black_box};
use types::channel::Slot;
use types::frame::Path;

use crate::log::{Found, Logs, Mark};
use crate::{create_logs, header};

pub(crate) fn lookup(logs: &Logs, slot: Slot, path: Path, from: Mark) -> Option<u64> {
    match logs.find(slot, path, from) {
        Found::Run(_, run) => Some(run.offset),
        Found::Trimmed(end) => Some(end.seq),
        Found::Nothing => None,
    }
}

/// PR only. Each commit trims one record and syncs one entry of each path in a new
/// record: each path drops one run and adds one. Twelve runs stay for each path.
#[divan::bench(args = [1, 64, 1024])]
fn trim_then_sync_new_record(bencher: Bencher<'_, '_>, paths: u32) {
    let mut logs = create_logs(paths, 12);
    let mut record = 12_u64;
    bencher.bench_local(|| {
        logs.trim(black_box(4096 * (record - 10)));
        for index in 0..paths {
            let header = header(index, record, 1);
            logs.sync(Slot::new(index), black_box(&header), 4096 * (record + 1))
                .expect("syncs");
        }
        record += 1;
    });
}

/// PR only. `lookup_in_order` after a trim that hid the older half of the runs,
/// with no sync after it. The marks go over the half that is left.
#[divan::bench(args = [16, 4096, 262_144])]
fn lookup_in_order_half_hidden(bencher: Bencher<'_, '_>, runs: u64) {
    let mut logs = create_logs(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1))
            .expect("syncs");
    }
    logs.trim(4096 * (runs / 2 + 1));
    let mut seq = runs / 2;
    bencher.bench_local(|| {
        let found = lookup(&logs, Slot::new(0), Path::Live, black_box(Mark::at(seq)));
        seq = if seq + 1 == runs { runs / 2 } else { seq + 1 };
        found
    });
}

/// PR only. A lookup from a mark in the hidden half: it gives the oldest run left.
#[divan::bench(args = [16, 4096, 262_144])]
fn lookup_from_hidden_half(bencher: Bencher<'_, '_>, runs: u64) {
    let mut logs = create_logs(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1))
            .expect("syncs");
    }
    logs.trim(4096 * (runs / 2 + 1));
    let mut seq = 0;
    bencher.bench_local(|| {
        let found = lookup(&logs, Slot::new(0), Path::Live, black_box(Mark::at(seq)));
        seq = if seq + 1 >= runs / 2 { 0 } else { seq + 1 };
        found
    });
}

/// PR only. A lookup of a path whose every run a trim hid: it gives the durable end.
#[divan::bench(args = [16, 4096, 262_144])]
fn lookup_all_hidden(bencher: Bencher<'_, '_>, runs: u64) {
    let mut logs = create_logs(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1))
            .expect("syncs");
    }
    logs.trim(4096 * (runs + 1));
    bencher.bench_local(|| lookup(&logs, Slot::new(0), Path::Live, black_box(Mark::at(0))));
}
