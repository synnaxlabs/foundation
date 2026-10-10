//! The [`Bytes`] allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt;
use std::sync::atomic::Ordering::{self, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicUsize};

/// A global allocator that counts the bytes it holds, so a test can bound the memory
/// of a structure. It counts no allocations: to assert that code does not allocate,
/// use [`Allocator`](crate::Allocator).
///
/// Each allocation that grows the count, [`Self::peak`], and [`Self::reset_peak`] take
/// one spin lock that every thread shares, so allocations on many threads wait for each
/// other.
///
/// ```
/// #[global_allocator]
/// static ALLOCATOR: counting::Bytes = counting::Bytes::new();
///
/// fn main() {
///     let before = ALLOCATOR.held();
///     let block = Box::new([0_u8; 64]);
///     assert_eq!(ALLOCATOR.held() - before, 64);
///     drop(block);
/// }
/// ```
pub struct Bytes {
    counts: Counts<AtomicUsize, AtomicBool>,
}

impl Bytes {
    /// Returns an allocator that holds no bytes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counts: Counts {
                held: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                locked: AtomicBool::new(false),
            },
        }
    }

    /// The bytes in the blocks that this allocator gave out and did not free, on every
    /// thread. A block counts its `Layout` size, not what the system rounds it up to.
    /// As the global allocator, read it in a binary with no test harness: a harness
    /// allocates on its own threads at any time.
    #[must_use]
    pub fn held(&self) -> usize {
        self.counts.held()
    }

    /// The most bytes held at once, on every thread, after the last
    /// [`Self::reset_peak`], or after the allocator started. As the global allocator,
    /// read it in a binary with no test harness, like [`Self::held`].
    #[must_use]
    pub fn peak(&self) -> usize {
        self.counts.peak()
    }

    /// Starts a new window of [`Self::peak`] at the bytes held now.
    pub fn reset_peak(&self) {
        self.counts.reset_peak();
    }

    /// Counts `size` bytes for `ptr` unless it is null, and returns it.
    fn counted(&self, ptr: *mut u8, size: usize) -> *mut u8 {
        if !ptr.is_null() {
            self.counts.grow(size);
        }
        ptr
    }
}

impl Default for Bytes {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.fmt(f)
    }
}

/// The counts of a [`Bytes`], generic over its atomics so that a loom model runs this
/// code on loom's.
struct Counts<W, F> {
    held: W,
    /// The most bytes held since the last reset. Only a holder of `locked` reads or
    /// writes it, so it is never under a `held` that a growth set.
    peak: W,
    locked: F,
}

impl<W: Word, F: Flag> Counts<W, F> {
    fn held(&self) -> usize {
        self.held.load(Relaxed)
    }

    fn peak(&self) -> usize {
        self.under_lock(|| self.peak.load(Relaxed))
    }

    fn reset_peak(&self) {
        self.under_lock(|| self.peak.store(self.held(), Relaxed));
    }

    fn grow(&self, size: usize) {
        self.under_lock(|| {
            let held = self.held.fetch_add(size, Relaxed) + size;
            self.peak.fetch_max(held, Relaxed);
        });
    }

    fn shrink(&self, size: usize) {
        self.held.fetch_sub(size, Relaxed);
    }

    /// Runs `f` while no other thread grows `held` or uses `peak`. A spin lock, as a
    /// `Mutex` can allocate on some targets. `f` must not allocate: the lock is not
    /// reentrant, so an allocation in `f` spins forever.
    fn under_lock<T>(&self, f: impl FnOnce() -> T) -> T {
        while self
            .locked
            .compare_exchange_weak(false, true, Acquire, Relaxed)
            .is_err()
        {
            F::spin();
        }
        let value = f();
        self.locked.store(false, Release);
        value
    }
}

impl<W: Word, F: Flag> fmt::Debug for Counts<W, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bytes")
            .field("held", &self.held())
            .field("peak", &self.peak())
            .finish()
    }
}

/// The `AtomicUsize` operations of [`Counts`].
trait Word {
    fn load(&self, order: Ordering) -> usize;
    fn store(&self, value: usize, order: Ordering);
    fn fetch_add(&self, value: usize, order: Ordering) -> usize;
    fn fetch_sub(&self, value: usize, order: Ordering) -> usize;
    fn fetch_max(&self, value: usize, order: Ordering) -> usize;
}

/// The `AtomicBool` operations of [`Counts`], and the hint of its spin loop.
trait Flag {
    fn compare_exchange_weak(
        &self,
        current: bool,
        new: bool,
        success: Ordering,
        failure: Ordering,
    ) -> Result<bool, bool>;
    fn store(&self, value: bool, order: Ordering);
    fn spin();
}

/// Implements [`Word`] and [`Flag`] for a pair of atomic types with the std names.
macro_rules! atomics {
    ($word:ty, $flag:ty, $spin:path) => {
        impl Word for $word {
            fn load(&self, order: Ordering) -> usize {
                <$word>::load(self, order)
            }
            fn store(&self, value: usize, order: Ordering) {
                <$word>::store(self, value, order);
            }
            fn fetch_add(&self, value: usize, order: Ordering) -> usize {
                <$word>::fetch_add(self, value, order)
            }
            fn fetch_sub(&self, value: usize, order: Ordering) -> usize {
                <$word>::fetch_sub(self, value, order)
            }
            fn fetch_max(&self, value: usize, order: Ordering) -> usize {
                <$word>::fetch_max(self, value, order)
            }
        }

        impl Flag for $flag {
            fn compare_exchange_weak(
                &self,
                current: bool,
                new: bool,
                success: Ordering,
                failure: Ordering,
            ) -> Result<bool, bool> {
                <$flag>::compare_exchange_weak(self, current, new, success, failure)
            }
            fn store(&self, value: bool, order: Ordering) {
                <$flag>::store(self, value, order);
            }
            fn spin() {
                $spin();
            }
        }
    };
}

atomics!(AtomicUsize, AtomicBool, std::hint::spin_loop);
#[cfg(all(test, loom))]
atomics!(
    loom::sync::atomic::AtomicUsize,
    loom::sync::atomic::AtomicBool,
    loom::hint::spin_loop
);

// SAFETY: each call passes its arguments to `System` under the same contract, and
// `held` changes no memory that `System` gives out.
unsafe impl GlobalAlloc for Bytes {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        self.counted(unsafe { System.alloc(layout) }, layout.size())
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        self.counted(unsafe { System.alloc_zeroed(layout) }, layout.size())
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::realloc`, and every
        // pointer this allocator returns comes from `System`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        // One change, so no thread reads a count that no state of the blocks had.
        if !new.is_null() {
            match new_size.checked_sub(layout.size()) {
                Some(grown) => self.counts.grow(grown),
                None => self.counts.shrink(layout.size() - new_size),
            }
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.counts.shrink(layout.size());
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`, and every
        // pointer this allocator returns comes from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use std::{slice, sync::atomic::AtomicPtr, thread};

    use super::*;

    const LAYOUT: Layout = Layout::new::<[u64; 8]>();

    /// A layout of `size` bytes at the alignment of [`LAYOUT`].
    fn sized(size: usize) -> Layout {
        Layout::from_size_align(size, LAYOUT.align())
            .expect("invariant: a small size at the alignment of `u64` is a layout")
    }

    /// A block of `LAYOUT` from `bytes`, with every byte `0xab`.
    fn filled(bytes: &Bytes) -> *mut u8 {
        // SAFETY: the layout is not empty.
        let ptr = unsafe { bytes.alloc(LAYOUT) };
        assert!(!ptr.is_null(), "the system has no memory for 64 bytes");
        // SAFETY: `ptr` holds `LAYOUT.size()` writable bytes.
        unsafe { ptr.write_bytes(0xab, LAYOUT.size()) };
        ptr
    }

    /// Frees `ptr`, which `bytes` returned for `layout`.
    fn free(bytes: &Bytes, ptr: *mut u8, layout: Layout) {
        // SAFETY: `bytes` returned `ptr` for `layout`, and nothing uses it after.
        unsafe { bytes.dealloc(ptr, layout) }
    }

    /// The first `len` bytes of `ptr`, which are initialized.
    fn read(ptr: *mut u8, len: usize) -> Vec<u8> {
        // SAFETY: the caller gives a block with `len` initialized bytes, and nothing
        // writes them while the slice lives.
        unsafe { slice::from_raw_parts(ptr, len) }.to_vec()
    }

    #[test]
    fn holds_the_size_of_an_allocation_until_its_free() {
        let bytes = Bytes::new();
        let ptr = filled(&bytes);
        assert_eq!(bytes.held(), 64);
        free(&bytes, ptr, LAYOUT);
        assert_eq!(bytes.held(), 0);
    }

    #[test]
    fn holds_the_size_of_a_zeroed_allocation() {
        let bytes = Bytes::new();
        // SAFETY: the layout is not empty.
        let ptr = unsafe { bytes.alloc_zeroed(LAYOUT) };
        assert!(!ptr.is_null(), "the system has no memory for 64 bytes");
        assert_eq!((bytes.held(), read(ptr, 64)), (64, vec![0; 64]));
        free(&bytes, ptr, LAYOUT);
        assert_eq!((bytes.held(), bytes.peak()), (0, 64));
    }

    #[test]
    fn holds_the_new_size_of_a_reallocation_that_grows() {
        let bytes = Bytes::new();
        let ptr = filled(&bytes);
        // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and 200 rounded up to the
        // alignment does not pass `isize::MAX`.
        let ptr = unsafe { bytes.realloc(ptr, LAYOUT, 200) };
        assert!(!ptr.is_null(), "the system has no memory for 200 bytes");
        assert_eq!((bytes.held(), read(ptr, 64)), (200, vec![0xab; 64]));
        free(&bytes, ptr, sized(200));
        assert_eq!(bytes.held(), 0);
    }

    #[test]
    fn holds_the_new_size_of_a_reallocation_that_shrinks() {
        let bytes = Bytes::new();
        let ptr = filled(&bytes);
        // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and 8 is not zero.
        let ptr = unsafe { bytes.realloc(ptr, LAYOUT, 8) };
        assert!(!ptr.is_null(), "the system has no memory for 8 bytes");
        assert_eq!((bytes.held(), read(ptr, 8)), (8, vec![0xab; 8]));
        free(&bytes, ptr, sized(8));
        assert_eq!(bytes.held(), 0);
    }

    // The trait's own `realloc` allocates before it frees, so it never gives back the
    // old block. glibc shrinks this block in place.
    #[test]
    #[cfg_attr(
        any(miri, not(all(target_os = "linux", target_env = "gnu"))),
        ignore = "only glibc is known to shrink this block in place"
    )]
    fn shrinks_a_block_in_place_as_the_system_does() {
        let bytes = Bytes::new();
        let ptr = filled(&bytes);
        // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and 56 is not zero.
        let shrunk = unsafe { bytes.realloc(ptr, LAYOUT, 56) };
        assert_eq!((shrunk, bytes.held()), (ptr, 56));
        free(&bytes, shrunk, sized(56));
    }

    #[test]
    fn holds_each_block_until_its_own_free() {
        let bytes = Bytes::new();
        let a = filled(&bytes);
        let b = filled(&bytes);
        assert_eq!(bytes.held(), 128);
        free(&bytes, a, LAYOUT);
        assert_eq!(bytes.held(), 64);
        free(&bytes, b, LAYOUT);
        assert_eq!(bytes.held(), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri stops at an allocation it cannot make")]
    fn does_not_hold_a_failed_allocation_or_reallocation() {
        let bytes = Bytes::new();
        let huge = isize::MAX.unsigned_abs() & !7;
        let layout = Layout::from_size_align(huge, 8)
            .expect("invariant: `isize::MAX` rounded down to 8 is a layout");
        // SAFETY: the layout is not empty.
        let ptr = unsafe { bytes.alloc(layout) };
        assert_eq!((ptr, bytes.held()), (std::ptr::null_mut(), 0));
        // SAFETY: the layout is not empty.
        let ptr = unsafe { bytes.alloc_zeroed(layout) };
        assert_eq!((ptr, bytes.held()), (std::ptr::null_mut(), 0));
        let ptr = filled(&bytes);
        // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and `huge` rounded up to the
        // alignment does not pass `isize::MAX`.
        let failed = unsafe { bytes.realloc(ptr, LAYOUT, huge) };
        assert_eq!((failed, bytes.held()), (std::ptr::null_mut(), 64));
        free(&bytes, ptr, LAYOUT);
        assert_eq!(bytes.held(), 0);
    }

    #[test]
    fn peaks_at_the_most_bytes_held_in_the_window() {
        let bytes = Bytes::new();
        let a = filled(&bytes);
        let b = filled(&bytes);
        free(&bytes, a, LAYOUT);
        assert_eq!((bytes.peak(), bytes.peak()), (128, 128));
        bytes.reset_peak();
        assert_eq!(bytes.peak(), 64);
        free(&bytes, b, LAYOUT);
        assert_eq!(bytes.peak(), 64);
        bytes.reset_peak();
        assert_eq!(bytes.peak(), 0);
    }

    #[test]
    fn peaks_at_a_reallocation_that_grows_and_not_one_that_shrinks() {
        let bytes = Bytes::new();
        let ptr = filled(&bytes);
        // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and 200 rounded up to the
        // alignment does not pass `isize::MAX`.
        let ptr = unsafe { bytes.realloc(ptr, LAYOUT, 200) };
        assert!(!ptr.is_null(), "the system has no memory for 200 bytes");
        // SAFETY: `bytes` returned `ptr` for 200 bytes, and 8 is not zero.
        let ptr = unsafe { bytes.realloc(ptr, sized(200), 8) };
        assert!(!ptr.is_null(), "the system has no memory for 8 bytes");
        assert_eq!(bytes.peak(), 200);
        bytes.reset_peak();
        assert_eq!(bytes.peak(), 8);
        free(&bytes, ptr, sized(8));
    }

    #[test]
    fn peaks_at_or_over_each_count_held_in_the_window_while_threads_allocate() {
        const TRIES: usize = if cfg!(miri) { 20 } else { 20_000 };
        let bytes = Bytes::new();
        let stopped = AtomicBool::new(false);
        let under = thread::scope(|scope| {
            for _ in 0..8 {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "a test owns its threads"
                )]
                scope.spawn(|| {
                    while !stopped.load(Relaxed) {
                        free(&bytes, filled(&bytes), LAYOUT);
                    }
                });
            }
            let under = (0..TRIES).find_map(|_| {
                bytes.reset_peak();
                let (held, peak) = (bytes.held(), bytes.peak());
                (peak < held).then_some((held, peak))
            });
            stopped.store(true, Relaxed);
            under
        });
        assert_eq!(under, None, "(held, peak) with the peak under a count held");
    }

    /// Holds the lock through the private field, and asserts that no call that reads
    /// or sets the peak finishes. The loom model runs [`Counts`], and the stress test
    /// sees only an `alloc` of [`Bytes`] that skips the lock, and only on enough cores.
    #[test]
    fn reads_and_sets_the_peak_only_under_the_lock() {
        let bytes = Bytes::new();
        let grown = AtomicPtr::new(filled(&bytes));
        bytes.counts.locked.store(true, Relaxed);
        let calls: [&(dyn Fn() + Sync); 6] = [
            &|| _ = bytes.peak(),
            &|| bytes.reset_peak(),
            &|| {
                drop(format!("{bytes:?}"));
            },
            &|| free(&bytes, filled(&bytes), LAYOUT),
            // SAFETY: the layout is not empty.
            &|| free(&bytes, unsafe { bytes.alloc_zeroed(LAYOUT) }, LAYOUT),
            &|| {
                let ptr = grown.load(Relaxed);
                // SAFETY: `bytes` returned `ptr` for `LAYOUT`, and only this call
                // uses it.
                let ptr = unsafe { bytes.realloc(ptr, LAYOUT, 200) };
                free(&bytes, ptr, sized(200));
            },
        ];
        let started = AtomicUsize::new(0);
        let finished = thread::scope(|scope| {
            #[expect(clippy::disallowed_methods, reason = "a test owns its threads")]
            let threads = calls.map(|call| {
                scope.spawn(|| {
                    started.fetch_add(1, Relaxed);
                    call();
                })
            });
            while started.load(Relaxed) < calls.len() {
                thread::yield_now();
            }
            for _ in 0..100 {
                thread::yield_now();
            }
            let finished = threads
                .each_ref()
                .map(thread::ScopedJoinHandle::is_finished);
            bytes.counts.locked.store(false, Release);
            finished
        });
        assert_eq!(
            finished, [false; 6],
            "(peak, reset_peak, Debug, alloc, alloc_zeroed, realloc) ended while locked"
        );
    }

    #[test]
    fn shows_the_bytes_held() {
        let bytes = Bytes::default();
        let ptr = filled(&bytes);
        assert_eq!(format!("{bytes:?}"), "Bytes { held: 64, peak: 64 }");
        free(&bytes, ptr, LAYOUT);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, AtomicUsize};
    use loom::thread;

    use super::*;

    /// The counts of a new [`Bytes`], on loom's atomics.
    fn counts() -> Counts<AtomicUsize, AtomicBool> {
        Counts {
            held: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            locked: AtomicBool::new(false),
        }
    }

    /// The two numbers of the `Debug` output of `counts`: held, then peak.
    fn shown(counts: &Counts<AtomicUsize, AtomicBool>) -> (usize, usize) {
        let shown = format!("{counts:?}");
        let mut numbers = shown
            .split(|c: char| !c.is_ascii_digit())
            .filter(|number| !number.is_empty())
            .map(|number| number.parse().expect("invariant: a run of digits"));
        match (numbers.next(), numbers.next(), numbers.next()) {
            (Some(held), Some(peak), None) => (held, peak),
            _ => panic!("`{shown}` does not show two counts"),
        }
    }

    /// Runs [`Counts`], not [`Bytes`]: `Bytes` keeps std's atomics for its `const`
    /// `new`, and a stress test on std threads makes the race only with enough cores.
    #[test]
    fn peaks_at_or_over_each_count_held_while_another_thread_allocates() {
        loom::model(|| {
            let counts = Arc::new(counts());
            let other = Arc::clone(&counts);
            let thread = thread::spawn(move || {
                other.grow(64);
                other.shrink(64);
            });
            counts.reset_peak();
            let held = counts.held();
            let (shown_held, shown_peak) = shown(&counts);
            let peak = counts.peak();
            thread.join().expect("the allocating thread does not panic");
            assert!(
                peak >= held,
                "the peak {peak} is under the {held} bytes held"
            );
            assert!(
                shown_peak >= shown_held,
                "`Debug` shows the peak {shown_peak} under the {shown_held} bytes held"
            );
        });
    }
}
