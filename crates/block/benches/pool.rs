//! Benchmarks of a pool: a block that is used again, a copy of given bytes, and a
//! block that takes the budget of another size under pressure.

use block::{Config, Heap, Pool};
use divan::Bencher;

fn main() {
    divan::main();
}

fn create_pool(budget: usize) -> Pool {
    let budget = u64::try_from(budget).expect("a usize fits in a u64");
    let config = Config::new(budget).expect("the budget fits");
    let heap = Heap::new(config.reservation());
    Pool::new(config, heap)
}

/// A block of one size, dropped and used again.
#[divan::bench]
fn alloc_free(bencher: Bencher<'_, '_>) {
    let pool = create_pool(1 << 16);
    bencher.bench_local(|| drop(pool.alloc(1000).expect("the budget has room")));
}

/// A frozen block of `len` given bytes, dropped and used again.
#[divan::bench(args = [16, 64, 1200, 16384])]
fn copy(bencher: Bencher<'_, '_>, len: usize) {
    let pool = create_pool(1 << 16);
    let bytes = vec![7_u8; len];
    bencher.bench_local(|| drop(pool.copy(&bytes).expect("the budget has room")));
}

/// A fresh block each time, with room for it. A full pool is replaced.
#[divan::bench]
fn carve(bencher: Bencher<'_, '_>) {
    const BUDGET: usize = (1 << 20) + 64;
    let mut pool = create_pool(BUDGET);
    let mut held = Vec::with_capacity(BUDGET / block::footprint(100) + 1);
    bencher.bench_local(|| {
        if let Ok(block) = pool.alloc(100) {
            held.push(block);
        } else {
            held.clear();
            pool = create_pool(BUDGET);
            held.push(pool.alloc(100).expect("the budget has room"));
        }
    });
}

/// `alloc_under_pressure` with a pool of 52 size classes.
#[divan::bench]
fn alloc_under_pressure_at_one_mebibyte(bencher: Bencher<'_, '_>) {
    let pool = create_pool(block::footprint(1 << 20));
    let mut small = false;
    bencher.bench_local(|| {
        small = !small;
        let len = if small { 100 } else { 1 << 20 };
        drop(pool.alloc(len).expect("the budget has room"));
    });
}

/// Blocks of two sizes in turn, with a budget for one: each alloc gives back the
/// range of the other size and carves its own again.
#[divan::bench]
fn alloc_under_pressure(bencher: Bencher<'_, '_>) {
    let pool = create_pool(block::footprint(1000));
    let mut small = false;
    bencher.bench_local(|| {
        small = !small;
        let len = if small { 100 } else { 1000 };
        drop(pool.alloc(len).expect("the budget has room"));
    });
}
