//! Scratch benchmark of the entry codec, for the review of PR 1793. Not for the repo.
#![allow(warnings, clippy::all, clippy::pedantic)]

#[path = "../../src/entry.rs"]
mod entry;

use divan::{Bencher, black_box};
use types::channel;
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;

fn main() {
    divan::main();
}

/// `count` headers, half on each path, every other one with a last stamp.
fn headers(count: usize) -> Vec<Header> {
    (0..count)
        .map(|at| Header {
            index: channel::Key::from_u128(at as u128),
            path: if at % 2 == 0 { Path::Live } else { Path::Backfill },
            first: at as u64,
            len: 1,
            stored_at: Stamp::from_nanos(7),
            last: (at % 2 == 0).then(|| Stamp::from_nanos(at as i64)),
            tag: 0,
            bytes: 0,
        })
        .collect()
}

/// `entry::write_table`: one `Header::encode` for each header.
#[divan::bench(args = [1, 64, 1023])]
fn write_table(bencher: Bencher<'_, '_>, count: usize) {
    let headers = headers(count);
    let mut into = vec![0; entry::table_len(count)];
    bencher.bench_local(|| entry::write_table(black_box(&headers), black_box(&mut into)));
}

/// `entry::parse`: one `Header::decode` for each header.
#[divan::bench(args = [1, 64, 1023])]
fn parse(bencher: Bencher<'_, '_>, count: usize) {
    let headers = headers(count);
    let mut table = vec![0; entry::table_len(count)];
    entry::write_table(&headers, &mut table);
    bencher.bench_local(|| {
        let mut sum = 0u64;
        for header in entry::parse(black_box(&table), table.len()).expect("parses") {
            let (header, _) = header.expect("decodes");
            sum = sum.wrapping_add(header.first);
        }
        sum
    });
}

/// Control: code that the PR does not change.
#[divan::bench]
fn control_crc32c(bencher: Bencher<'_, '_>) {
    let bytes = vec![7u8; 4096];
    bencher.bench_local(|| crc32c::crc32c(black_box(&bytes)));
}
