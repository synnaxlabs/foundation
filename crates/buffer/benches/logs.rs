//! The time to sync the durable logs of a shard at each commit, for each entry: one
//! record with one data entry of each index. With a tag, each log first holds a
//! tagged entry, as a log of a home holds a handoff.

use std::num::NonZeroU8;

use buffer::bench::Logs;
use divan::Bencher;
use divan::counter::ItemsCount;

fn main() {
    divan::main();
}

#[divan::bench(args = [1, 64, 256])]
fn sync(bencher: Bencher<'_, '_>, indexes: u32) {
    cycle(bencher, indexes, None);
}

#[divan::bench(args = [1, 64, 256])]
fn sync_after_tag(bencher: Bencher<'_, '_>, indexes: u32) {
    cycle(bencher, indexes, Some(NonZeroU8::MIN));
}

fn cycle(bencher: Bencher<'_, '_>, indexes: u32, tag: Option<NonZeroU8>) {
    let mut logs = Logs::new(indexes, tag);
    // Times logs that hold their runs, not logs still filling.
    for _ in 0..16 {
        logs.commit();
    }
    bencher
        .counter(ItemsCount::new(
            usize::try_from(indexes).expect("u32 fits usize"),
        ))
        .bench_local(|| logs.commit());
}
