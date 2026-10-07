//! Scratch benchmark of the drop of hidden runs in `Logs::sync`, for the review of
//! PR 1554. Head only. Not for the repo.
#![allow(warnings, clippy::all, clippy::pedantic)]

#[path = "../../src/entry.rs"]
mod entry;
#[path = "../../src/log.rs"]
mod log;

use divan::{Bencher, black_box};
use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;
use crate::log::Logs;

fn main() {
    divan::main();
}

fn header(first: u64) -> Header {
    Header {
        index: channel::Key::from_u128(0),
        path: Path::Live,
        first,
        len: 1,
        stored_at: Stamp::from_nanos(7),
        last: Some(Stamp::from_nanos(7)),
        tag: 0,
        bytes: 0,
    }
}

/// One sync of a new record on a path with `hidden` hidden runs and five runs
/// left: the sync drops each hidden run. The deque has room for the new run.
#[divan::bench(args = [0, 1, 16, 256, 4096, 65536])]
fn sync_drops_hidden(bencher: Bencher<'_, '_>, hidden: u64) {
    bencher
        .with_inputs(|| {
            let mut logs = Logs::default();
            for record in 0..hidden + 5 {
                logs.sync(Slot::new(0), &header(record), 4096 * (record + 1))
                    .expect("syncs");
            }
            logs.hide(4096 * (hidden + 1));
            logs
        })
        .bench_local_refs(|logs| {
            logs.sync(
                Slot::new(0),
                black_box(&header(hidden + 5)),
                black_box(4096 * (hidden + 6)),
            )
            .expect("syncs");
        });
}
