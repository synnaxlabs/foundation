//! The time of `wait::Set::insert` of a waker that the set holds for the last of `n`
//! keys, the scan that each pending poll of a waiter pays.

use std::hint::black_box;
use std::task::Waker;

use divan::Bencher;
use types::wait::Set;

fn main() {
    divan::main();
}

#[divan::bench(args = [1, 63])]
fn insert_held(bencher: Bencher<'_, '_>, n: u64) {
    let mut set = Set::new();
    for key in 0..n {
        drop(set.insert(key, Waker::noop()));
    }
    bencher.bench_local(|| {
        black_box(set.insert(black_box(n - 1), Waker::noop()));
    });
}
