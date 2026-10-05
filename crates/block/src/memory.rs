//! The address space a pool cuts its blocks from.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::fmt;
use std::ptr::NonNull;

use crate::ALIGN;

/// A reserved range of address space that one [`Pool`](crate::Pool) cuts blocks from.
///
/// The pool calls [`commit`](Self::commit) before it first uses a range, and
/// [`purge`](Self::purge) when a range is idle. Offsets are from
/// [`base`](Self::base). A range need not be page-aligned: `commit` rounds it out, and
/// `purge` rounds it in.
///
/// # Safety
///
/// - `base` and `len` give the same values on each call. The `len` bytes at `base`
///   belong to this value alone, at one address, until it drops.
/// - When `commit` returns, each byte in its range is readable, writable, and
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
    fn commit(&self, offset: usize, len: usize);

    /// Lets the system take back the pages that lie fully in `len` bytes at `offset`.
    /// A pool calls it under budget pressure, and for a range that is idle.
    fn purge(&self, offset: usize, len: usize);
}

/// Memory from the heap, committed in full from the start. It never gives pages back.
/// It serves tests and simulation, where no OS mapping exists.
pub struct Heap {
    base: NonNull<u8>,
    layout: Layout,
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
        let Ok(layout) = Layout::from_size_align(len, ALIGN) else {
            panic!("heap memory of {len} bytes is too large to allocate");
        };
        // SAFETY: the layout has a size above 0.
        let base = unsafe { alloc_zeroed(layout) };
        let Some(base) = NonNull::new(base) else {
            handle_alloc_error(layout)
        };
        Self { base, layout }
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
        self.layout.size()
    }

    fn commit(&self, _offset: usize, _len: usize) {}

    fn purge(&self, _offset: usize, _len: usize) {}
}

impl fmt::Debug for Heap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heap").field("len", &self.len()).finish()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // SAFETY: `new` got `base` from the allocator with this layout.
        unsafe { dealloc(self.base.as_ptr(), self.layout) };
    }
}
