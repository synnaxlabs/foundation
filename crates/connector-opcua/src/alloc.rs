//! The allocator of the open62541 copy, `shim.c`, and `server.c`. `src/alloc.h`
//! defines `UA_malloc`, `UA_calloc`, `UA_realloc`, and `UA_free` as these functions,
//! so each C allocation goes through the global allocator of the binary. Each keeps
//! the C contract: a failure gives NULL and never panics, and each pointer is aligned
//! to 16. A free writes zeros over its block, as aws-lc does in its default mode, so a
//! freed block keeps no copy of a key or a password. [`exhausted`] stops the process
//! when a constructor of `shim.c` gives NULL.
//! Behind `sim`, [`route_crypto`] gives these functions to aws-lc too.

#![expect(unsafe_code, reason = "C allocates through these functions")]

use std::alloc::{self, Layout};
use std::ffi::c_void;
#[cfg(feature = "sim")]
use std::ffi::{c_char, c_int};
use std::{ptr, slice};

/// The alignment of each block, the `max_align_t` of each target, and the size of the
/// header before it, which holds the size that C asked for.
const ALIGN: usize = 16;

const _: () = assert!(size_of::<usize>() <= ALIGN, "the size fits in the header");

/// The layout of a header alone, the smallest block.
const HEADER: Layout = match Layout::from_size_align(ALIGN, ALIGN) {
    Ok(layout) => layout,
    Err(_) => panic!("a header alone is a layout"),
};

/// Stops the process as the Rust allocator does when it has no memory, for a C
/// constructor that gives NULL. A constructor does not give the size of the block that
/// failed, so the size in the message is that of a header alone, the smallest block
/// that this allocator asks for, not the size of the block that failed.
pub(crate) fn exhausted() -> ! {
    alloc::handle_alloc_error(HEADER)
}

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

/// Gives the block of `ptr` and its layout. The header is then unpoisoned: the caller
/// frees the block, or calls `poison` when the block stays live.
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

/// Gives the size that C asked for in the block of `ptr`.
///
/// # Safety
///
/// As [`block`].
unsafe fn asked(ptr: *mut c_void) -> usize {
    // SAFETY: as the caller.
    let (block, layout) = unsafe { block(ptr) };
    poison(block);
    layout.size() - ALIGN
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
/// `ptr` and gives a pointer as `malloc(0)` does. It always moves the block, and frees
/// the old one as `free` does. On a failure it gives NULL and `ptr` stays live.
///
/// # Safety
///
/// `ptr` is NULL or a live pointer of these functions.
#[unsafe(no_mangle)]
unsafe extern "C" fn connector_opcua_realloc(
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    let new = connector_opcua_malloc(size);
    if ptr.is_null() || new.is_null() {
        return new;
    }
    // SAFETY: the caller gives a live pointer of these functions.
    let count = unsafe { asked(ptr) }.min(size);
    // SAFETY: `ptr` and `new` are two live blocks of at least `count` bytes.
    unsafe { ptr::copy_nonoverlapping(ptr.cast::<u8>(), new.cast::<u8>(), count) };
    // SAFETY: as above.
    unsafe { connector_opcua_free(ptr) };
    new
}

/// `free`. `free(NULL)` does nothing.
///
/// # Safety
///
/// `ptr` is NULL or a live pointer of these functions.
#[unsafe(no_mangle)]
unsafe extern "C" fn connector_opcua_free(ptr: *mut c_void) {
    // SAFETY: as the caller.
    unsafe { connector_opcua_free_written(ptr, usize::MAX) };
}

/// `free` for a block of which C wrote only the first `written` bytes: it clears only
/// those, so a large buffer that holds a short message costs only the message.
///
/// # Safety
///
/// As `connector_opcua_free`.
#[unsafe(no_mangle)]
unsafe extern "C" fn connector_opcua_free_written(ptr: *mut c_void, written: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the caller gives a live pointer of these functions.
    let (block, layout) = unsafe { block(ptr) };
    let size = written.min(layout.size() - ALIGN);
    // SAFETY: `ptr` is at least `size` bytes of the block after its header.
    unsafe { ptr.cast::<u8>().write_bytes(0, size) };
    // SAFETY: the bytes are live and now hold zeros.
    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), size) };
    // The `dealloc` frees the bytes, so the compiler may drop writes that nothing reads.
    zeroize::optimization_barrier(bytes);
    // SAFETY: `block` is a live block of `layout`, from the global allocator.
    unsafe { alloc::dealloc(block, layout) };
}

/// Makes `OPENSSL_malloc`, `OPENSSL_realloc`, and `OPENSSL_free` of aws-lc use the
/// global allocator of the binary, so a counting allocator counts the blocks of aws-lc
/// too, other than the blocks that aws-lc takes from `malloc` itself: the array of its
/// thread locals, one block for each thread, and its error queue (the state of each
/// thread that puts an error, and the data of each error). aws-lc then does not clear a
/// block before it frees it.
///
/// # Safety
///
/// When no earlier call routed aws-lc, no block of aws-lc is live: call it before the
/// first call of aws-lc in the process. The free of an older block reads a header that
/// it does not have.
///
/// # Panics
///
/// When aws-lc already has the functions of another call of
/// `CRYPTO_set_mem_functions`.
#[cfg(feature = "sim")]
pub unsafe fn route_crypto() {
    extern "C" fn crypto_malloc(
        size: usize,
        _: *const c_char,
        _: c_int,
    ) -> *mut c_void {
        connector_opcua_malloc(size)
    }
    /// # Safety
    ///
    /// `ptr` is NULL or a live block of `crypto_malloc` or `crypto_realloc`.
    unsafe extern "C" fn crypto_realloc(
        ptr: *mut c_void,
        size: usize,
        _: *const c_char,
        _: c_int,
    ) -> *mut c_void {
        // SAFETY: the caller gives NULL or a live block of these functions.
        unsafe { connector_opcua_realloc(ptr, size) }
    }
    /// # Safety
    ///
    /// As `crypto_realloc`.
    unsafe extern "C" fn crypto_free(ptr: *mut c_void, _: *const c_char, _: c_int) {
        // SAFETY: as in `crypto_realloc`.
        unsafe { connector_opcua_free(ptr) }
    }
    // SAFETY: aws-lc keeps the three functions, which live for the process, and the
    // caller ensures that no block of aws-lc is live.
    let set = unsafe {
        aws_lc_sys::CRYPTO_set_mem_functions(
            Some(crypto_malloc),
            Some(crypto_realloc),
            Some(crypto_free),
        )
    };
    assert!(set == 1, "aws-lc already has the functions of another call");
}

#[cfg(test)]
mod tests;
