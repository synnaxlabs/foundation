//! The hot path makes no heap allocation.

#![expect(unsafe_code, reason = "a counting allocator implements `GlobalAlloc`")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::pin::pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll, Waker};

use ring::{Config, Full};

/// Counts the allocations of the process.
struct Counting {
    count: AtomicU64,
}

// SAFETY: every call goes to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.count.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting {
    count: AtomicU64::new(0),
};

/// Allocations the process makes while `f` runs.
fn count(f: impl FnOnce()) -> u64 {
    let before = ALLOCATOR.count.load(Relaxed);
    f();
    ALLOCATOR.count.load(Relaxed) - before
}

// The count covers every thread, so this binary holds one test: a second test would
// allocate while this one counts.
#[test]
fn push_try_pop_and_pop_do_not_allocate() {
    assert_eq!(count(|| drop(Box::new(1_u8))), 1, "the allocator counts");

    let (mut producer, mut consumer) = ring::new(Config {
        capacity: 4,
        spins: 0,
    });
    let mut cx = Context::from_waker(Waker::noop());
    let allocations = count(|| {
        for value in 0..64_u64 {
            assert_eq!(producer.push(value), Ok(()));
            assert_eq!(producer.push(value), Ok(()));
            assert_eq!(consumer.try_pop(), Some(value));
            assert_eq!(consumer.try_pop(), Some(value));
            assert_eq!(consumer.try_pop(), None);
        }
        for value in 0..4_u64 {
            assert_eq!(producer.push(value), Ok(()));
        }
        assert_eq!(producer.push(4), Err(Full(4)));
        for value in 0..4_u64 {
            assert_eq!(consumer.try_pop(), Some(value));
        }
        // A park, a wake, and a pop.
        for value in 0..64_u64 {
            assert_eq!(pin!(consumer.pop()).poll(&mut cx), Poll::Pending);
            assert_eq!(producer.push(value), Ok(()));
            assert_eq!(pin!(consumer.pop()).poll(&mut cx), Poll::Ready(Some(value)));
        }
    });
    assert_eq!(allocations, 0, "the hot path allocated");
}
