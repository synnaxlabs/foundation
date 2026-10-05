//! The hot path makes no heap allocation. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(unsafe_code, reason = "a counting allocator implements `GlobalAlloc`")]
#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

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

    let config = Config { budget: 2 * 4160 };
    let heap = Heap::new(config.reservation());
    let pool = Pool::new(config, heap);
    let large = [pool.alloc(4096), pool.alloc(4096)].map(|block| block.expect("room"));
    drop(large);
    let allocations = count(|| {
        for _ in 0..64 {
            drop(pool.alloc(1).expect("a freed large block gives its budget"));
        }
    });
    assert_eq!(allocations, 0, "the pressure path allocated");
    assert_eq!(pool.committed(), 128, "the idle large size gave its budget");

    let allocations = count(|| {
        assert_eq!(
            pool.purge(),
            0,
            "the small size returned a block this interval"
        );
        assert_eq!(pool.purge(), 128, "the small size stayed idle");
    });
    assert_eq!(allocations, 0, "the purge path allocated");
    assert_eq!(pool.committed(), 0, "each idle size gave its pages back");
}
