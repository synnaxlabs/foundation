//! Counts heap allocations, so a test can assert that code does not allocate.

#![expect(unsafe_code, reason = "the allocator implements `GlobalAlloc`")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

/// An allocator that passes each call to [`System`] and counts the allocations. Each
/// `alloc`, `alloc_zeroed`, and `realloc` counts as one; `dealloc` does not count.
///
/// ```
/// #[global_allocator]
/// static ALLOCATOR: counting::Allocator = counting::Allocator::new();
///
/// fn main() {
///     let (sum, allocations) = ALLOCATOR.count(|| 2 + 2);
///     assert_eq!((sum, allocations), (4, 0));
/// }
/// ```
#[derive(Debug, Default)]
pub struct Allocator {
    allocations: AtomicU64,
}

impl Allocator {
    /// Returns an allocator that has counted nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
        }
    }

    /// Runs `f` and returns its result and the allocations made through this allocator
    /// while it ran, on every thread. As the global allocator, run it in a binary with
    /// no test harness: a harness allocates on its own threads at any time.
    pub fn count<T>(&self, f: impl FnOnce() -> T) -> (T, u64) {
        let before = self.allocations.load(Relaxed);
        let value = f();
        let after = self.allocations.load(Relaxed);
        (value, after.strict_sub(before))
    }
}

// SAFETY: each method passes its arguments to `System` unchanged, so it keeps the
// contract that `System` keeps.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.allocations.fetch_add(1, Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::realloc`, and every
        // pointer this allocator returns comes from `System`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, and every
        // pointer this allocator returns comes from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::*;

    const LAYOUT: Layout = Layout::new::<[u64; 8]>();

    /// Frees `ptr`, which `allocator` returned for `layout`, and checks that the free
    /// does not count.
    fn free(allocator: &Allocator, ptr: *mut u8, layout: Layout) {
        let ((), frees) = allocator.count(|| {
            // SAFETY: `allocator` returned `ptr` for `layout`, and nothing uses it after.
            unsafe { allocator.dealloc(ptr, layout) }
        });
        assert_eq!(frees, 0, "a free does not count");
    }

    #[test]
    fn counts_an_allocation_of_usable_memory() {
        let allocator = Allocator::new();
        let (ptr, allocations) = allocator.count(|| {
            // SAFETY: the layout is not empty.
            unsafe { allocator.alloc(LAYOUT) }
        });
        assert_eq!(allocations, 1);
        assert!(!ptr.is_null(), "the system has no memory for 64 bytes");
        // SAFETY: `ptr` holds `LAYOUT.size()` writable bytes.
        unsafe { ptr.write_bytes(0xab, LAYOUT.size()) };
        free(&allocator, ptr, LAYOUT);
    }

    #[test]
    fn counts_a_zeroed_allocation() {
        let allocator = Allocator::new();
        let (ptr, allocations) = allocator.count(|| {
            // SAFETY: the layout is not empty.
            unsafe { allocator.alloc_zeroed(LAYOUT) }
        });
        assert_eq!(allocations, 1);
        assert!(!ptr.is_null(), "the system has no memory for 64 bytes");
        // SAFETY: `ptr` holds `LAYOUT.size()` initialized bytes, and nothing writes them
        // while the slice lives.
        let bytes = unsafe { slice::from_raw_parts(ptr, LAYOUT.size()) };
        assert_eq!(bytes, [0; 64]);
        free(&allocator, ptr, LAYOUT);
    }

    #[test]
    fn counts_a_reallocation_that_keeps_the_bytes() {
        let allocator = Allocator::new();
        // SAFETY: the layout is not empty.
        let ptr = unsafe { allocator.alloc(LAYOUT) };
        assert!(!ptr.is_null(), "the system has no memory for 64 bytes");
        // SAFETY: `ptr` holds `LAYOUT.size()` writable bytes.
        unsafe { ptr.write_bytes(0xab, LAYOUT.size()) };
        let (ptr, allocations) = allocator.count(|| {
            // SAFETY: `allocator` returned `ptr` for `LAYOUT`, and 128 rounded up to the
            // alignment does not pass `isize::MAX`.
            unsafe { allocator.realloc(ptr, LAYOUT, 128) }
        });
        assert_eq!(allocations, 1);
        assert!(!ptr.is_null(), "the system has no memory for 128 bytes");
        // SAFETY: the first `LAYOUT.size()` bytes of `ptr` are initialized, and nothing
        // writes them while the slice lives.
        let bytes = unsafe { slice::from_raw_parts(ptr, LAYOUT.size()) };
        assert_eq!(bytes, [0xab; 64]);
        let layout = Layout::from_size_align(128, LAYOUT.align())
            .expect("invariant: 128 bytes at the alignment of `u64` is a layout");
        free(&allocator, ptr, layout);
    }

    #[test]
    fn returns_the_result_of_the_closure() {
        assert_eq!(Allocator::new().count(|| "result"), ("result", 0));
    }

    #[test]
    fn shows_the_count() {
        let allocator = Allocator::new();
        // SAFETY: the layout is not empty.
        let ptr = unsafe { allocator.alloc(LAYOUT) };
        assert_eq!(format!("{allocator:?}"), "Allocator { allocations: 1 }");
        free(&allocator, ptr, LAYOUT);
    }
}
