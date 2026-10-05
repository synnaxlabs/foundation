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
}

/// One box made on a thread that started before the count counts once.
fn counts_other_threads() {
    let [ready, go, done] = [const { AtomicBool::new(false) }; 3];
    let wait = |flag: &AtomicBool| {
        while !flag.load(Acquire) {
            thread::yield_now();
        }
    };
    thread::scope(|scope| {
        scope.spawn(|| {
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
    });
}
