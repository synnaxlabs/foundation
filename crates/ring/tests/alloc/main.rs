//! The hot path makes no heap allocation. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(unsafe_code, reason = "a counting allocator implements `GlobalAlloc`")]
#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll, Wake, Waker};

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

/// A waker with a reference count, as a task has. It counts its wakes.
#[derive(Default)]
struct Tally(AtomicU64);

impl Wake for Tally {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Relaxed);
    }
}

fn main() {
    assert_eq!(count(|| drop(Box::new(1_u8))), 1, "the allocator counts");

    let (mut producer, mut consumer) = ring::new(Config {
        capacity: 4,
        spins: 0,
    });
    let tallies = [Arc::new(Tally::default()), Arc::new(Tally::default())];
    let wakers = tallies
        .each_ref()
        .map(|tally| Waker::from(Arc::clone(tally)));
    let allocations = count(|| {
        for value in 0..64_u64 {
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
            assert_eq!(consumer.try_pop(), Some(value), "values come in order");
            assert_eq!(consumer.try_pop(), Some(value), "values come in order");
            assert_eq!(consumer.try_pop(), None, "the ring is empty");
        }
        for value in 0..4_u64 {
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
        }
        assert_eq!(producer.push(4), Err(Full(4)), "the ring is full");
        for value in 0..4_u64 {
            assert_eq!(consumer.try_pop(), Some(value), "values come in order");
        }
        for (value, waker) in (0..64_u64).zip(wakers.iter().cycle()) {
            let mut cx = Context::from_waker(waker);
            let parked = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(parked, Poll::Pending, "the ring is empty");
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
            let popped = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(popped, Poll::Ready(Some(value)), "the push woke the pop");
        }
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    let counts = tallies.each_ref().map(|tally| tally.0.load(Relaxed));
    assert_eq!(counts, [32, 32], "each park got one wake");
}
