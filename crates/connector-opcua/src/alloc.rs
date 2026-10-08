//! The allocator of the open62541 copy and `shim.c`. `src/alloc.h` defines
//! `UA_malloc`, `UA_calloc`, `UA_realloc`, and `UA_free` as these functions, so each C
//! allocation goes through the global allocator of the binary. Each keeps the C
//! contract: a failure gives NULL and never panics, and each pointer is aligned to 16.

#![expect(unsafe_code, reason = "C allocates through these functions")]

use std::alloc::{self, Layout};
use std::ffi::c_void;
use std::ptr;

/// The size of the header before each block, which holds the size that C asked for,
/// and the alignment of each block: the `max_align_t` of each target.
const HEADER: usize = 16;

/// Gives the layout of a block of `size` bytes and its header, or `None` when it is
/// too large.
fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
}

/// Writes `size` into the header of `block` and gives the pointer after the header,
/// or NULL when `block` is NULL.
///
/// # Safety
///
/// `block` is NULL or a live block of `layout(size)`.
#[expect(clippy::cast_ptr_alignment, reason = "each block is aligned to 16")]
unsafe fn give(block: *mut u8, size: usize) -> *mut c_void {
    if block.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the block starts with `HEADER` bytes, aligned to 16.
    unsafe { block.cast::<usize>().write(size) };
    // SAFETY: the block holds at least `HEADER` bytes.
    unsafe { block.add(HEADER) }.cast()
}

/// Gives the block of `ptr` and its layout.
///
/// # Safety
///
/// `ptr` is a live pointer that `give` gave.
#[expect(clippy::cast_ptr_alignment, reason = "each block is aligned to 16")]
unsafe fn block(ptr: *mut c_void) -> (*mut u8, Layout) {
    // SAFETY: `give` put the header just before `ptr`, in the same block.
    let block = unsafe { ptr.cast::<u8>().sub(HEADER) };
    // SAFETY: `give` wrote the size there.
    let size = unsafe { block.cast::<usize>().read() };
    // SAFETY: `layout(size)` was valid when the block was made.
    let layout = unsafe { Layout::from_size_align_unchecked(size + HEADER, HEADER) };
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
    unsafe { give(block, size) }
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
    unsafe { give(block, size) }
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
    let (block, old) = unsafe { block(ptr) };
    // SAFETY: `block` is a live block of `old`, from the global allocator, and `new`
    // has the same alignment and a valid size.
    let block = unsafe { alloc::realloc(block, old, new.size()) };
    // SAFETY: `block` is NULL or a block of `new`.
    unsafe { give(block, size) }
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
