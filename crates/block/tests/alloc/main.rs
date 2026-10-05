//! The hot path makes no heap allocation. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(unsafe_code, reason = "a counting allocator implements `GlobalAlloc`")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use block::{Config, Heap, Pool};

/// Counts the allocations and the frees of the process.
struct Counting {
    count: AtomicU64,
    freed: AtomicU64,
}

// SAFETY: every call goes to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.count.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.freed.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting {
    count: AtomicU64::new(0),
    freed: AtomicU64::new(0),
};

/// Allocations the process makes while `f` runs.
fn count(f: impl FnOnce()) -> u64 {
    let before = ALLOCATOR.count.load(Relaxed);
    f();
    ALLOCATOR.count.load(Relaxed) - before
}

fn main() {
    assert_eq!(count(|| drop(Box::new(1_u8))), 1, "the allocator counts");
    let freed = ALLOCATOR.freed.load(Relaxed);
    drop(Heap::new(64));
    assert_eq!(
        ALLOCATOR.freed.load(Relaxed) - freed,
        1,
        "a heap frees its bytes"
    );

    let config = Config { budget: 1 << 16 };
    let heap = Heap::new(config.reservation());
    let pool = Pool::new(config, heap);
    let allocations = count(|| {
        for len in [0, 1, 64, 65, 1000, 4096].into_iter().cycle().take(600) {
            let mut unique = pool.alloc(len).expect("the budget has room");
            unique.fill(1);
            let block = unique.freeze();
            let clone = block.clone();
            assert_eq!(clone.len(), len, "a clone has the same bytes");
            drop(block);
            drop(clone);
            drop(pool.alloc(len).expect("the budget has room"));
            pool.reclaim();
        }
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    // One block of each of the four classes: a dropped block serves the next alloc.
    let committed = 128 + 192 + 1088 + 4160;
    assert_eq!(pool.committed(), committed, "blocks are used again");
}
