//! The allocator of the open62541 copy and `shim.c`. `src/alloc.h` defines
//! `UA_malloc`, `UA_calloc`, `UA_realloc`, and `UA_free` as these functions, so each C
//! allocation goes through the global allocator of the binary. Each keeps the C
//! contract: a failure gives NULL and never panics, and each pointer is aligned to 16.

#![expect(unsafe_code, reason = "C allocates through these functions")]

use std::alloc::{self, Layout};
use std::ffi::c_void;
use std::ptr;

/// The alignment of each block, the `max_align_t` of each target, and the size of the
/// header before it, which holds the size that C asked for.
const ALIGN: usize = 16;

const _: () = assert!(size_of::<usize>() <= ALIGN, "the size fits in the header");

/// Gives the layout of a block of `size` bytes and its header, or `None` when it is
/// too large.
fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(ALIGN)?, ALIGN).ok()
}

#[cfg(asan)]
unsafe extern "C" {
    fn __asan_poison_memory_region(addr: *const c_void, size: usize);
    fn __asan_unpoison_memory_region(addr: *const c_void, size: usize);
}

/// Makes the address sanitizer report each access to the header of `block`, so a C
/// write just before a block is an error, as one past it is.
#[cfg(asan)]
fn poison(block: *mut u8) {
    // SAFETY: the call only marks the bytes, which are in `block`.
    unsafe { __asan_poison_memory_region(block.cast(), ALIGN) };
}

#[cfg(not(asan))]
fn poison(_: *mut u8) {}

/// Lets this module read and free the header of `block` again.
#[cfg(asan)]
fn unpoison(block: *mut u8) {
    // SAFETY: as in `poison`.
    unsafe { __asan_unpoison_memory_region(block.cast(), ALIGN) };
}

#[cfg(not(asan))]
fn unpoison(_: *mut u8) {}

/// Writes `size` into the header of `block` and gives the pointer after the header,
/// or NULL when `block` is NULL.
///
/// # Safety
///
/// `block` is NULL or a live block of `layout(size)`.
#[expect(clippy::cast_ptr_alignment, reason = "each block is aligned to 16")]
unsafe fn stamp(block: *mut u8, size: usize) -> *mut c_void {
    if block.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the block starts with a header of `ALIGN` bytes, aligned to `ALIGN`.
    unsafe { block.cast::<usize>().write(size) };
    poison(block);
    // SAFETY: the block holds at least its header.
    unsafe { block.add(ALIGN) }.cast()
}

/// Gives the block of `ptr` and its layout.
///
/// # Safety
///
/// `ptr` is a live pointer that `stamp` gave.
#[expect(clippy::cast_ptr_alignment, reason = "each block is aligned to 16")]
unsafe fn block(ptr: *mut c_void) -> (*mut u8, Layout) {
    // SAFETY: `stamp` put the header just before `ptr`, in the same block.
    let block = unsafe { ptr.cast::<u8>().sub(ALIGN) };
    unpoison(block);
    // SAFETY: `stamp` wrote the size there.
    let size = unsafe { block.cast::<usize>().read() };
    // SAFETY: `layout(size)` was valid when the block was made.
    let layout = unsafe { Layout::from_size_align_unchecked(size + ALIGN, ALIGN) };
    (block, layout)
}

/// `malloc`. `malloc(0)` gives a unique pointer, not NULL.
#[unsafe(no_mangle)]
extern "C" fn connector_opcua_malloc(size: usize) -> *mut c_void {
    let Some(layout) = layout(size) else {
        return ptr::null_mut();
    };
    // SAFETY: the layout is not empty.
    let block = unsafe { alloc::alloc(layout) };
    // SAFETY: `block` is NULL or a block of `layout(size)`.
    unsafe { stamp(block, size) }
}

/// `calloc`. It gives NULL when `count * size` overflows.
#[unsafe(no_mangle)]
extern "C" fn connector_opcua_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(size) = count.checked_mul(size) else {
        return ptr::null_mut();
    };
    let Some(layout) = layout(size) else {
        return ptr::null_mut();
    };
    // SAFETY: the layout is not empty.
    let block = unsafe { alloc::alloc_zeroed(layout) };
    // SAFETY: `block` is NULL or a block of `layout(size)`.
    unsafe { stamp(block, size) }
}

/// `realloc`. `realloc(NULL, size)` is `malloc(size)`, and `realloc(ptr, 0)` frees
/// `ptr` and gives a pointer as `malloc(0)` does. On a failure it gives NULL and
/// `ptr` stays live.
///
/// # Safety
///
/// `ptr` is NULL or a live pointer of these functions.
#[unsafe(no_mangle)]
unsafe extern "C" fn connector_opcua_realloc(
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    if ptr.is_null() {
        return connector_opcua_malloc(size);
    }
    let Some(new) = layout(size) else {
        return ptr::null_mut();
    };
    // SAFETY: the caller gives a live pointer of these functions.
    let (old_block, old) = unsafe { block(ptr) };
    // SAFETY: `old_block` is a live block of `old`, from the global allocator, and
    // `new` has the same alignment and a valid size.
    let block = unsafe { alloc::realloc(old_block, old, new.size()) };
    if block.is_null() {
        // `ptr` stays live.
        poison(old_block);
    }
    // SAFETY: `block` is NULL or a block of `new`.
    unsafe { stamp(block, size) }
}

/// `free`. `free(NULL)` does nothing.
///
/// # Safety
///
/// `ptr` is NULL or a live pointer of these functions.
#[unsafe(no_mangle)]
unsafe extern "C" fn connector_opcua_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the caller gives a live pointer of these functions.
    let (block, layout) = unsafe { block(ptr) };
    // SAFETY: `block` is a live block of `layout`, from the global allocator.
    unsafe { alloc::dealloc(block, layout) };
}

#[cfg(test)]
mod tests;
