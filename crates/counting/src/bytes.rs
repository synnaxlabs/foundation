//! The [`Bytes`] allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

/// A global allocator that counts the bytes it holds, so a test can bound the memory
/// of a structure. It counts no allocations: to assert that code does not allocate,
/// use [`Allocator`](crate::Allocator).
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
    held: AtomicUsize,
}

impl Bytes {
    /// Returns an allocator that holds no bytes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: AtomicUsize::new(0),
        }
    }

    /// The bytes in the blocks that this allocator gave out and did not free, on every
    /// thread. A block counts its `Layout` size, not what the system rounds it up to.
    /// As the global allocator, read it in a binary with no test harness: a harness
    /// allocates on its own threads at any time.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.load(Relaxed)
    }

    /// Counts `size` bytes for `ptr` unless it is null, and returns it.
    fn counted(&self, ptr: *mut u8, size: usize) -> *mut u8 {
        if !ptr.is_null() {
            self.held.fetch_add(size, Relaxed);
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
        f.debug_struct("Bytes").field("held", &self.held).finish()
    }
}

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
        let new =
            self.counted(unsafe { System.realloc(ptr, layout, new_size) }, new_size);
        if !new.is_null() {
            self.held.fetch_sub(layout.size(), Relaxed);
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.held.fetch_sub(layout.size(), Relaxed);
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
        assert_eq!(bytes.held(), 0);
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
    fn shows_the_bytes_held() {
        let bytes = Bytes::default();
        let ptr = filled(&bytes);
        assert_eq!(format!("{bytes:?}"), "Bytes { held: 64 }");
        free(&bytes, ptr, LAYOUT);
    }
}
