//! As the global allocator, the count covers every thread. This binary has no test
//! harness, because a harness allocates on its own threads at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::hint::black_box;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::thread;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    let ((), allocations) = ALLOCATOR.count(|| drop(black_box(Box::new(1_u8))));
    assert_eq!(allocations, 1, "a box allocates once");
    let (sum, allocations) = ALLOCATOR.count(|| black_box(2_u64) + 2);
    assert_eq!((sum, allocations), (4, 0), "arithmetic does not allocate");
    counts_other_threads();
    let before = ALLOCATOR.held();
    let block = black_box(Box::new([0_u8; 64]));
    assert_eq!(
        ALLOCATOR.held().strict_sub(before),
        64,
        "a box holds its size"
    );
    drop(block);
    assert_eq!(ALLOCATOR.held(), before, "a freed box holds nothing");
    let needle = [0xab; 32];
    let ((), found) =
        ALLOCATOR.freed_holding(&needle, || drop(black_box(Box::new(needle))));
    assert_eq!(found, 1, "a freed box that holds the needle counts");
}

/// One box made on a thread that started before the count counts once.
#[expect(
    clippy::disallowed_methods,
    reason = "a thread test; `counting` has no `env`"
)]
fn counts_other_threads() {
    let [ready, go, done] = [const { AtomicBool::new(false) }; 3];
    let wait = |flag: &AtomicBool| {
        while !flag.load(Acquire) {
            thread::yield_now();
        }
    };
    thread::scope(|scope| {
        let thread = scope.spawn(|| {
            ready.store(true, Release);
            wait(&go);
            drop(black_box(Box::new(1_u8)));
            done.store(true, Release);
        });
        wait(&ready);
        let ((), allocations) = ALLOCATOR.count(|| {
            go.store(true, Release);
            wait(&done);
        });
        assert_eq!(allocations, 1, "the other thread's box counts");
        // The scope waits for the closure, not for the exit of the thread, which frees
        // blocks that a later `freed_holding` scans.
        thread.join().expect("the thread does not panic");
    });
}
