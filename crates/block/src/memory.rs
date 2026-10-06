//! The address space a pool cuts its blocks from.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::fmt;
use std::ptr::NonNull;

use crate::ALIGN;

/// A reserved range of address space that one [`Pool`](crate::Pool) cuts blocks from.
///
/// The first [`ALIGN`] bytes are usable from the start. The pool calls
/// [`commit`](Self::commit) before it uses any other range, first or after a purge,
/// and [`purge`](Self::purge) when a range is idle. Offsets are from
/// [`base`](Self::base). A range need not be page-aligned: `commit` rounds it out, and
/// `purge` rounds it in.
///
/// # Safety
///
/// - `base` and `len` give the same values on each call. The `len` bytes at `base`
///   belong to this value alone, at one address, until it drops.
/// - The first [`ALIGN`] bytes are as if a `commit` of them returned `Ok` before the
///   first call.
/// - When `commit` returns `Ok`, each byte in its range is readable, writable, and
///   initialized. It stays so until a `purge` of that byte or the drop.
/// - A `purge` may change the bytes in its range. It changes no other byte.
#[expect(
    clippy::len_without_is_empty,
    reason = "a reserved range is never empty"
)]
pub unsafe trait Memory: Send {
    /// The first byte of the range.
    fn base(&self) -> NonNull<u8>;

    /// The length of the range in bytes.
    fn len(&self) -> usize;

    /// Makes `len` bytes at `offset` usable.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the system has no memory for the range now. The range stays
    /// unusable, and a later call may succeed.
    fn commit(&self, offset: usize, len: usize) -> Result<(), Refused>;

    /// Lets the system take back the pages that lie fully in `len` bytes at `offset`.
    /// Those pages then stop counting against the memory the system can commit.
    fn purge(&self, offset: usize, len: usize);
}

/// The system has no memory to commit a range now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

/// Memory from the heap, usable in full from the start. It never gives pages back.
/// It serves tests and simulation, where no OS mapping exists.
pub struct Heap {
    /// What the allocator gave. `base` rounds it up to [`ALIGN`].
    allocation: NonNull<u8>,
    layout: Layout,
    base: NonNull<u8>,
}

impl Heap {
    /// Allocates `len` zeroed bytes aligned to [`ALIGN`].
    ///
    /// # Panics
    ///
    /// If `len` is 0 or too large to allocate.
    #[must_use]
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "heap memory must be more than 0 bytes");
        // Std reaches `calloc`, which leaves the pages untouched, only at the
        // allocator's own alignment. At `ALIGN` it writes zero over every page.
        let Some(layout) = len
            .checked_add(ALIGN)
            .and_then(|size| Layout::from_size_align(size, 1).ok())
        else {
            panic!("heap memory of {len} bytes is too large to allocate");
        };
        // SAFETY: the layout has a size above 0.
        let allocation = unsafe { alloc_zeroed(layout) };
        let Some(allocation) = NonNull::new(allocation) else {
            handle_alloc_error(layout)
        };
        let addr = allocation.addr().get();
        let skew = addr.next_multiple_of(ALIGN) - addr;
        // SAFETY: `skew` is under `ALIGN`, so `base` and the `len` bytes after it
        // lie in the allocation.
        let base = unsafe { allocation.add(skew) };
        Self {
            allocation,
            layout,
            base,
        }
    }
}

// SAFETY: `Heap` owns its allocation, and the allocator lets any thread free it.
unsafe impl Send for Heap {}

// SAFETY: the allocation is zeroed, so each byte is initialized from the start. It
// stays at one address until the drop, and `purge` changes nothing.
unsafe impl Memory for Heap {
    fn base(&self) -> NonNull<u8> {
        self.base
    }

    fn len(&self) -> usize {
        self.layout.size() - ALIGN
    }

    fn commit(&self, _offset: usize, _len: usize) -> Result<(), Refused> {
        Ok(())
    }

    fn purge(&self, _offset: usize, _len: usize) {}
}

impl fmt::Debug for Heap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heap").field("len", &self.len()).finish()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // SAFETY: `new` got `allocation` from the allocator with this layout.
        unsafe { dealloc(self.allocation.as_ptr(), self.layout) };
    }
}
