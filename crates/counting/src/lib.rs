//! Counts heap allocations, so a test can assert that code does not allocate, and
//! finds freed blocks that hold given bytes, so a test can assert that code erases a
//! secret before it frees it.

#![expect(unsafe_code, reason = "the allocator implements `GlobalAlloc`")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize};

/// The longest needle [`Allocator::freed_holding`] takes.
const NEEDLE: usize = 32;

/// An allocator that gets its memory from [`System`] and counts the allocations. Each
/// `alloc`, `alloc_zeroed`, and `realloc` that succeeds counts as one; a failed one and
/// `dealloc` do not count. A `realloc` always allocates a new block, copies, and frees
/// the old block.
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
#[derive(Default)]
pub struct Allocator {
    allocations: AtomicU64,
    scan: Scan,
}

impl Allocator {
    /// Returns an allocator that has counted nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
            scan: Scan {
                len: AtomicUsize::new(0),
                needle: [const { AtomicU8::new(0) }; NEEDLE],
                found: AtomicU64::new(0),
            },
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

    /// Runs `f` and returns its result and the number of blocks freed through this
    /// allocator while it ran, on every thread, that held `needle` when freed. The old
    /// block of a `realloc` counts too. As the global allocator, run it in a binary
    /// with no test harness, like [`Self::count`].
    ///
    /// # Panics
    ///
    /// If `needle` is empty or longer than 32 bytes, or if another call runs.
    pub fn freed_holding<T>(&self, needle: &[u8], f: impl FnOnce() -> T) -> (T, u64) {
        let before = self.scan.found.load(Relaxed);
        let running = self.scan.start(needle);
        let value = f();
        drop(running);
        let after = self.scan.found.load(Relaxed);
        (value, after.strict_sub(before))
    }

    /// Counts `ptr` unless it is null, and returns it.
    fn counted(&self, ptr: *mut u8) -> *mut u8 {
        if !ptr.is_null() {
            self.allocations.fetch_add(1, Relaxed);
        }
        ptr
    }
}

impl fmt::Debug for Allocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Allocator")
            .field("allocations", &self.allocations)
            .finish_non_exhaustive()
    }
}

/// The state of [`Allocator::freed_holding`].
#[derive(Default)]
struct Scan {
    /// The length of the needle while a call runs, [`WRITING`] while one writes the
    /// needle, else 0.
    len: AtomicUsize,
    /// A copy, because a free on another thread can read it after the call returns.
    needle: [AtomicU8; NEEDLE],
    found: AtomicU64,
}

/// The length of the needle while a call writes it.
const WRITING: usize = usize::MAX;

impl Scan {
    fn start(&self, needle: &[u8]) -> Running<'_> {
        assert!(
            (1..=NEEDLE).contains(&needle.len()),
            "a needle holds 1 to {NEEDLE} bytes, not {}",
            needle.len()
        );
        assert!(
            self.len
                .compare_exchange(0, WRITING, Relaxed, Relaxed)
                .is_ok(),
            "calls to `freed_holding` do not overlap"
        );
        for (byte, value) in self.needle.iter().zip(needle) {
            byte.store(*value, Relaxed);
        }
        self.len.store(needle.len(), Release);
        Running(self)
    }

    /// Counts the block of `size` bytes at `block` if a call runs and the block holds
    /// its needle.
    ///
    /// # Safety
    ///
    /// `block` is valid for reads of `size` bytes.
    unsafe fn check(&self, block: *const u8, size: usize) {
        let len = self.len.load(Acquire);
        if !(1..=NEEDLE).contains(&len) {
            return;
        }
        let mut needle = [0; NEEDLE];
        for (byte, value) in needle.iter_mut().zip(&self.needle) {
            *byte = value.load(Relaxed);
        }
        let Some(last) = size.checked_sub(len) else {
            return;
        };
        let holds = (0..=last).any(|start| {
            needle.iter().take(len).zip(start..).all(|(value, index)| {
                let byte = block.wrapping_add(index);
                // SAFETY: `index` is below `size`, so `byte` is in the block. A
                // volatile read keeps the compiler from assuming the value of a byte
                // the program never wrote, such as padding.
                unsafe { byte.read_volatile() == *value }
            })
        });
        if holds {
            self.found.fetch_add(1, Relaxed);
        }
    }
}

/// Ends a scan when dropped, also when the closure panics.
struct Running<'a>(&'a Scan);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.len.store(0, Release);
    }
}

// SAFETY: every block comes from `System`, and each method keeps the contract of the
// `System` call it makes. `realloc` is the trait's own, which calls `alloc` and
// `dealloc`.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        self.counted(unsafe { System.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        self.counted(unsafe { System.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, so `ptr`
        // holds `layout.size()` bytes.
        unsafe { self.scan.check(ptr, layout.size()) };
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, and every
        // pointer this allocator returns comes from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use std::{panic, slice};

    use super::*;

    const LAYOUT: Layout = Layout::new::<[u64; 8]>();

    const SECRET: [u8; 32] = *b"0123456789abcdefghijklmnopqrstuv";

    /// Frees `ptr`, which `allocator` returned for `layout`, and checks that the free
    /// does not count.
    fn free(allocator: &Allocator, ptr: *mut u8, layout: Layout) {
        let ((), frees) = allocator.count(|| {
            // SAFETY: `allocator` returned `ptr` for `layout`, and nothing uses it
            // after.
            unsafe { allocator.dealloc(ptr, layout) }
        });
        assert_eq!(frees, 0, "a free does not count");
    }

    /// A block of `layout` from `allocator` that holds zeros, then `bytes` at its end.
    fn block(allocator: &Allocator, layout: Layout, bytes: &[u8]) -> *mut u8 {
        // SAFETY: the layout is not empty.
        let ptr = unsafe { allocator.alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "the system has no memory for {layout:?}");
        // SAFETY: `ptr` holds `layout.size()` initialized bytes, and nothing else uses
        // them while the slice lives.
        let block = unsafe { slice::from_raw_parts_mut(ptr, layout.size()) };
        let start = block.len().strict_sub(bytes.len());
        block.split_at_mut(start).1.copy_from_slice(bytes);
        ptr
    }

    /// Frees `ptr`, which `allocator` returned for `layout`, and returns whether the
    /// block held [`SECRET`].
    fn found(allocator: &Allocator, ptr: *mut u8, layout: Layout) -> u64 {
        let ((), found) = allocator.freed_holding(&SECRET, || {
            // SAFETY: `allocator` returned `ptr` for `layout`, and nothing uses it
            // after.
            unsafe { allocator.dealloc(ptr, layout) }
        });
        found
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
    #[cfg_attr(miri, ignore = "Miri stops at an allocation it cannot make")]
    fn does_not_count_a_failed_allocation() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(isize::MAX.unsigned_abs() & !7, 8)
            .expect("invariant: `isize::MAX` rounded down to 8 is a layout");
        let (ptr, allocations) = allocator.count(|| {
            // SAFETY: the layout is not empty.
            unsafe { allocator.alloc(layout) }
        });
        assert_eq!((ptr, allocations), (std::ptr::null_mut(), 0));
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
        // SAFETY: `ptr` holds `LAYOUT.size()` initialized bytes, and nothing writes
        // them while the slice lives.
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
            // SAFETY: `allocator` returned `ptr` for `LAYOUT`, and 128 rounded up to
            // the alignment does not pass `isize::MAX`.
            unsafe { allocator.realloc(ptr, LAYOUT, 128) }
        });
        assert_eq!(allocations, 1);
        assert!(!ptr.is_null(), "the system has no memory for 128 bytes");
        // SAFETY: the first `LAYOUT.size()` bytes of `ptr` are initialized, and nothing
        // writes them while the slice lives.
        let bytes = unsafe { slice::from_raw_parts(ptr, LAYOUT.size()) };
        assert_eq!(bytes, [0xab; 64]);
        // SAFETY: `ptr` holds 128 writable bytes.
        unsafe { ptr.write_bytes(0xcd, 128) };
        // SAFETY: the 128 bytes of `ptr` are initialized, and nothing writes them while
        // the slice lives.
        let bytes = unsafe { slice::from_raw_parts(ptr, 128) };
        assert_eq!(bytes, [0xcd; 128]);
        let layout = Layout::from_size_align(128, LAYOUT.align())
            .expect("invariant: 128 bytes at the alignment of `u64` is a layout");
        free(&allocator, ptr, layout);
    }

    #[test]
    fn counts_a_freed_block_that_ends_with_the_needle() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, &SECRET);
        assert_eq!(found(&allocator, ptr, LAYOUT), 1);
    }

    #[test]
    fn does_not_count_a_block_that_holds_part_of_the_needle() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, &SECRET[..31]);
        assert_eq!(found(&allocator, ptr, LAYOUT), 0);
    }

    #[test]
    fn does_not_count_a_block_shorter_than_the_needle() {
        let allocator = Allocator::new();
        let layout = Layout::new::<[u8; 16]>();
        let ptr = block(&allocator, layout, &SECRET[..16]);
        assert_eq!(found(&allocator, ptr, layout), 0);
    }

    #[test]
    fn counts_the_old_block_of_a_reallocation() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, &SECRET);
        let ((ptr, found), allocations) = allocator.count(|| {
            allocator.freed_holding(&SECRET, || {
                // SAFETY: `allocator` returned `ptr` for `LAYOUT`, and 128 rounded up
                // to the alignment does not pass `isize::MAX`.
                unsafe { allocator.realloc(ptr, LAYOUT, 128) }
            })
        });
        assert_eq!((found, allocations), (1, 1));
        assert!(!ptr.is_null(), "the system has no memory for 128 bytes");
        // SAFETY: the first `LAYOUT.size()` bytes of `ptr` are initialized, and nothing
        // writes them while the slice lives.
        let bytes = unsafe { slice::from_raw_parts(ptr, LAYOUT.size()) };
        assert_eq!(bytes.split_at(32), (&[0; 32][..], &SECRET[..]));
        let layout = Layout::from_size_align(128, LAYOUT.align())
            .expect("invariant: 128 bytes at the alignment of `u64` is a layout");
        free(&allocator, ptr, layout);
    }

    #[test]
    fn ends_the_scan_when_the_closure_panics() {
        let allocator = Allocator::new();
        panic::catch_unwind(|| {
            allocator.freed_holding(&SECRET, || panic!("the closure"))
        })
        .expect_err("the closure panics");
        let ptr = block(&allocator, LAYOUT, &SECRET);
        assert_eq!(found(&allocator, ptr, LAYOUT), 1);
    }

    #[test]
    #[should_panic(expected = "a needle holds 1 to 32 bytes, not 0")]
    fn panics_on_an_empty_needle() {
        Allocator::new().freed_holding(&[], || ());
    }

    #[test]
    #[should_panic(expected = "a needle holds 1 to 32 bytes, not 33")]
    fn panics_on_a_needle_over_32_bytes() {
        Allocator::new().freed_holding(&[0; 33], || ());
    }

    #[test]
    #[should_panic(expected = "calls to `freed_holding` do not overlap")]
    fn panics_on_a_call_inside_a_call() {
        let allocator = Allocator::new();
        allocator.freed_holding(&SECRET, || allocator.freed_holding(&SECRET, || ()));
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
        assert_eq!(format!("{allocator:?}"), "Allocator { allocations: 1, .. }");
        free(&allocator, ptr, LAYOUT);
    }
}
