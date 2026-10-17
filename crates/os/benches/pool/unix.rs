use block::{Config, Pool};
use divan::Bencher;
use os::memory::Memory;

const SMALL: usize = 8000;
const LARGE: usize = 16_384;

fn create_pool(budget: usize) -> Pool {
    let budget = u64::try_from(budget).expect("a usize fits in a u64");
    let config = Config::new(budget).expect("the budget fits");
    let memory = Memory::new(config.reservation()).expect("the system has the range");
    Pool::new(config, memory)
}

/// Blocks of two sizes in turn, with a budget for one: each alloc purges the range of
/// the other size, commits its own again, and writes it.
#[divan::bench]
fn alloc_under_pressure(bencher: Bencher<'_, '_>) {
    let pool = create_pool(block::footprint(LARGE));
    let mut small = false;
    bencher.bench_local(|| {
        small = !small;
        let len = if small { SMALL } else { LARGE };
        pool.alloc(len).expect("the budget has room").fill(1);
    });
}

/// Blocks of two sizes in turn, with a budget for two large blocks while one of them
/// stays in use: the small alloc discards the payload pages of the free large block,
/// and the large alloc purges the small size and takes that block back. Each alloc
/// writes its block.
#[divan::bench]
fn alloc_cold(bencher: Bencher<'_, '_>) {
    let pool = create_pool(2 * block::footprint(LARGE));
    let held = pool.alloc(LARGE).expect("the budget has room");
    drop(pool.alloc(LARGE).expect("the budget has room"));
    let mut small = false;
    bencher.bench_local(|| {
        small = !small;
        let len = if small { SMALL } else { LARGE };
        pool.alloc(len)
            .expect("the free block gives its pages")
            .fill(1);
    });
    drop(held);
}

/// One alloc that discards the payload pages of many free blocks, of which about one
/// in four holds a whole page.
#[divan::bench(args = [16, 256, 1024], sample_count = 30, sample_size = 1)]
fn alloc_cools(bencher: Bencher<'_, '_>, blocks: usize) {
    const SMALL: usize = 5000;
    let large = (blocks - 1) * 4096 / 4 / 5;
    bencher
        .with_inputs(|| {
            let pool = create_pool(blocks * block::footprint(SMALL));
            let mut held: Vec<_> = (0..blocks)
                .map(|_| {
                    let mut block = pool.alloc(SMALL).expect("the budget has room");
                    block.fill(1);
                    block
                })
                .collect();
            let kept = held.pop().expect("a block");
            drop(held);
            (pool, kept)
        })
        .bench_local_refs(|(pool, _)| {
            pool.alloc(large).expect("the free blocks give their pages")
        });
}
