//! Counts heap allocations, so a test can assert that code does not allocate, counts
//! the heap bytes held, so a test can bound the memory of a structure, and finds freed
//! blocks that hold given bytes, so a test can assert that code erases a
//! secret before it frees it.

#![expect(unsafe_code, reason = "the allocator implements `GlobalAlloc`")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize};
use std::{fmt, hint, ptr, slice};

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
pub struct Allocator {
    allocations: AtomicU64,
    held: AtomicUsize,
    scan: Scan,
}

impl Allocator {
    /// Returns an allocator that has counted nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
            held: AtomicUsize::new(0),
            scan: Scan::new(),
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

    /// The bytes in the blocks that this allocator gave out and did not free, on every
    /// thread. A block counts its `Layout` size, not what the system rounds it up to.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.load(Relaxed)
    }

    /// Runs `f` and returns its result and the number of blocks freed through this
    /// allocator while it ran, on every thread, that held `needle` when freed. The old
    /// block of a `realloc` counts too. A free on another thread that starts while `f`
    /// runs counts, and the call waits for it to end. As the global allocator, run it
    /// in a binary with no test harness, like [`Self::count`]. A freed block can hold
    /// bytes the program never wrote, such as padding or the spare capacity of a
    /// `Vec`. Rust does not define a read of such a byte on any target, and Miri stops
    /// at one.
    ///
    /// # Panics
    ///
    /// If `needle` is empty, or if another call runs.
    pub fn freed_holding<T>(&self, needle: &[u8], f: impl FnOnce() -> T) -> (T, u64) {
        let running = self.scan.start(needle);
        let value = f();
        (value, running.end())
    }

    /// Counts `ptr`, a block of `layout`, unless it is null, and returns it.
    fn counted(&self, ptr: *mut u8, layout: Layout) -> *mut u8 {
        if !ptr.is_null() {
            self.allocations.fetch_add(1, Relaxed);
            self.held.fetch_add(layout.size(), Relaxed);
        }
        ptr
    }
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Allocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Allocator")
            .field("allocations", &self.allocations)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

/// The state of [`Allocator::freed_holding`].
struct Scan {
    /// The phase in the bits of [`PHASE`], and the number of frees in [`Scan::search`]
    /// as a multiple of [`SCANNER`].
    state: AtomicUsize,
    /// The caller's needle, which lives until the phase leaves [`RUNNING`] and no free
    /// scans.
    needle: AtomicPtr<u8>,
    len: AtomicUsize,
    found: AtomicU64,
}

/// No call runs.
const IDLE: usize = 0;
/// A call runs, and frees scan for its needle.
const RUNNING: usize = 1;
/// A call starts or ends, and frees do not scan.
const BUSY: usize = 2;
const PHASE: usize = 3;
const SCANNER: usize = 4;

impl Scan {
    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(IDLE),
            needle: AtomicPtr::new(ptr::null_mut()),
            len: AtomicUsize::new(0),
            found: AtomicU64::new(0),
        }
    }

    fn start<'a>(&'a self, needle: &'a [u8]) -> Running<'a> {
        assert!(!needle.is_empty(), "a needle holds at least one byte");
        let idle = self.state.fetch_update(Acquire, Relaxed, |state| {
            (state & PHASE == IDLE).then_some(state + BUSY)
        });
        assert!(idle.is_ok(), "calls to `freed_holding` do not overlap");
        self.needle.store(needle.as_ptr().cast_mut(), Relaxed);
        self.len.store(needle.len(), Relaxed);
        self.state.fetch_sub(BUSY - RUNNING, Release);
        Running(self)
    }

    /// Stops new scans, waits for the running ones, and returns the blocks found.
    fn end(&self) -> u64 {
        self.state.fetch_add(BUSY - RUNNING, Relaxed);
        while self.state.load(Acquire) != BUSY {
            hint::spin_loop();
        }
        let found = self.found.swap(0, Relaxed);
        self.state.fetch_sub(BUSY, Release);
        found
    }

    /// Counts `block` if a call runs and the block holds its needle.
    fn check(&self, block: &[MaybeUninit<u8>]) {
        if self.state.load(Relaxed) & PHASE == RUNNING {
            self.search(block);
        }
    }

    /// Counts `block` if a call still runs and the block holds its needle. A free
    /// calls it after it saw a call run, which may have ended since.
    fn search(&self, block: &[MaybeUninit<u8>]) {
        if self.state.fetch_add(SCANNER, Acquire) & PHASE == RUNNING {
            // SAFETY: the call that set the needle holds its borrow until `end`, which
            // waits for this scan.
            let needle = unsafe {
                slice::from_raw_parts(self.needle.load(Relaxed), self.len.load(Relaxed))
            };
            let holds = block.windows(needle.len()).any(|window| {
                window.iter().zip(needle).all(|(byte, value)| {
                    // SAFETY: `byte` is a `u8` in the block. Rust does not define a
                    // read of a byte the program never wrote, such as padding. A
                    // volatile read is one load that the compiler cannot remove or
                    // assume a value for, which is as close as Rust allows.
                    unsafe { byte.as_ptr().read_volatile() == *value }
                })
            });
            if holds {
                self.found.fetch_add(1, Relaxed);
            }
        }
        self.state.fetch_sub(SCANNER, Release);
    }
}

/// Ends a scan, also when the closure panics, because a free on another thread can
/// still read the caller's needle.
struct Running<'a>(&'a Scan);

impl Running<'_> {
    /// Ends the scan in place of the drop, and returns the blocks found.
    fn end(self) -> u64 {
        ManuallyDrop::new(self).0.end()
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

// SAFETY: every block comes from `System`, and each method keeps the contract of the
// `System` call it makes. `realloc` is the trait's own, which calls `alloc` and
// `dealloc`.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        self.counted(unsafe { System.alloc(layout) }, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        self.counted(unsafe { System.alloc_zeroed(layout) }, layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, so `ptr`
        // holds `layout.size()` bytes, and nothing writes them before the free.
        let block = unsafe {
            slice::from_raw_parts(ptr.cast::<MaybeUninit<u8>>(), layout.size())
        };
        self.scan.check(block);
        self.held.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, and every
        // pointer this allocator returns comes from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use std::{panic, thread};

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

    /// A zeroed block of `layout` from `allocator`, with `bytes` from `start`.
    fn block(
        allocator: &Allocator,
        layout: Layout,
        start: usize,
        bytes: &[u8],
    ) -> *mut u8 {
        // SAFETY: the layout is not empty.
        let ptr = unsafe { allocator.alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "the system has no memory for {layout:?}");
        // SAFETY: `ptr` holds `layout.size()` initialized bytes, and nothing else uses
        // them while the slice lives.
        let block = unsafe { slice::from_raw_parts_mut(ptr, layout.size()) };
        block[start..start.strict_add(bytes.len())].copy_from_slice(bytes);
        ptr
    }

    /// Frees `ptr`, which `allocator` returned for `layout`, and returns the count of
    /// blocks that held `needle`.
    fn found(
        allocator: &Allocator,
        needle: &[u8],
        ptr: *mut u8,
        layout: Layout,
    ) -> u64 {
        let ((), found) = allocator.freed_holding(needle, || {
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
    fn holds_the_layout_size_of_each_block_until_it_is_freed() {
        let allocator = Allocator::new();
        // SAFETY: the layout is not empty.
        let a = unsafe { allocator.alloc(LAYOUT) };
        // SAFETY: the layout is not empty.
        let b = unsafe { allocator.alloc_zeroed(LAYOUT) };
        assert!(
            !a.is_null() && !b.is_null(),
            "the system has no memory for 64 bytes"
        );
        assert_eq!(allocator.held(), 128);
        // SAFETY: `allocator` returned `a` for `LAYOUT`, and 200 rounded up to the
        // alignment does not pass `isize::MAX`.
        let a = unsafe { allocator.realloc(a, LAYOUT, 200) };
        assert!(!a.is_null(), "the system has no memory for 200 bytes");
        assert_eq!(allocator.held(), 264);
        free(&allocator, b, LAYOUT);
        assert_eq!(allocator.held(), 200);
        let layout = Layout::from_size_align(200, LAYOUT.align())
            .expect("invariant: 200 bytes at the alignment of `u64` is a layout");
        free(&allocator, a, layout);
        assert_eq!(allocator.held(), 0);
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
        assert_eq!(allocator.held(), 0);
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
    fn counts_a_freed_block_that_holds_the_needle_at_any_offset() {
        let allocator = Allocator::new();
        for start in 0..=32 {
            let ptr = block(&allocator, LAYOUT, start, &SECRET);
            assert_eq!(found(&allocator, &SECRET, ptr, LAYOUT), 1, "at {start}");
        }
    }

    #[test]
    fn counts_a_needle_of_one_byte_and_of_the_whole_block() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, 63, &[0xff]);
        assert_eq!(found(&allocator, &[0xff], ptr, LAYOUT), 1);
        let needle = [SECRET, SECRET].concat();
        let ptr = block(&allocator, LAYOUT, 0, &needle);
        assert_eq!(found(&allocator, &needle, ptr, LAYOUT), 1);
    }

    #[test]
    fn does_not_count_a_block_that_holds_part_of_the_needle() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, 32, &SECRET[..31]);
        assert_eq!(found(&allocator, &SECRET, ptr, LAYOUT), 0);
    }

    #[test]
    fn does_not_count_a_block_shorter_than_the_needle() {
        let allocator = Allocator::new();
        let layout = Layout::new::<[u8; 16]>();
        let ptr = block(&allocator, layout, 0, &SECRET[..16]);
        assert_eq!(found(&allocator, &SECRET, ptr, layout), 0);
    }

    #[test]
    fn does_not_count_a_block_freed_outside_the_call() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, 32, &SECRET);
        free(&allocator, ptr, LAYOUT);
        assert_eq!(allocator.freed_holding(&SECRET, || ()), ((), 0));
    }

    /// A free on another thread that starts while the closure runs counts in that
    /// call, also when it ends after the closure returns.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a thread test; `counting` has no `env`"
    )]
    fn counts_a_free_on_another_thread_that_ends_after_the_closure() {
        let allocator = Allocator::new();
        // Under Miri, a block small enough to scan in time. Elsewhere, one that takes
        // far longer to scan than the closure takes to return.
        let size = if cfg!(miri) { 64 } else { 8 << 20 };
        let layout = Layout::from_size_align(size, 8).expect("a layout");
        let ptr =
            AtomicPtr::new(block(&allocator, layout, size.strict_sub(32), &SECRET));
        thread::scope(|scope| {
            let (freer, found) = allocator.freed_holding(&SECRET, || {
                let freer = scope.spawn(|| {
                    // SAFETY: `allocator` returned `ptr` for `layout`, and nothing
                    // uses it after.
                    unsafe { allocator.dealloc(ptr.load(Relaxed), layout) }
                });
                while allocator.scan.state.load(Relaxed) < SCANNER
                    && !freer.is_finished()
                {
                    hint::spin_loop();
                }
                freer
            });
            freer.join().expect("the free does not panic");
            assert_eq!(found, 1);
        });
    }

    /// A free that saw the call run, and searches after it ended.
    #[test]
    fn does_not_count_a_search_after_the_call() {
        let scan = Scan::new();
        scan.start(&SECRET).end();
        scan.search(&SECRET.map(MaybeUninit::new));
        assert_eq!(scan.start(&SECRET).end(), 0);
    }

    #[test]
    fn counts_the_old_block_of_a_reallocation() {
        let allocator = Allocator::new();
        let ptr = block(&allocator, LAYOUT, 32, &SECRET);
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
        let ptr = block(&allocator, LAYOUT, 32, &SECRET);
        assert_eq!(found(&allocator, &SECRET, ptr, LAYOUT), 1);
    }

    #[test]
    #[should_panic(expected = "a needle holds at least one byte")]
    fn panics_on_an_empty_needle() {
        Allocator::new().freed_holding(&[], || ());
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
    fn shows_the_counts() {
        let allocator = Allocator::new();
        // SAFETY: the layout is not empty.
        let ptr = unsafe { allocator.alloc(LAYOUT) };
        assert_eq!(
            format!("{allocator:?}"),
            "Allocator { allocations: 1, held: 64, .. }"
        );
        free(&allocator, ptr, LAYOUT);
    }
}
