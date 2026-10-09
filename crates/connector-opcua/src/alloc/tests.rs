#![expect(unsafe_code, reason = "the tests call the C functions")]

use std::ffi::c_void;

use super::{
    connector_opcua_calloc as calloc, connector_opcua_free as free,
    connector_opcua_malloc as malloc, connector_opcua_realloc as realloc,
};

/// Asserts that `ptr` is a block for C, aligned to 16.
fn assert_aligned(ptr: *mut c_void) {
    assert!(!ptr.is_null());
    assert_eq!(ptr.addr() % 16, 0, "{:#x}", ptr.addr() % 16);
}

/// A size that passes the checks of these functions but that no allocator gives.
const HUGE: usize = isize::MAX.unsigned_abs() - 64;

#[test]
fn malloc_of_0_gives_a_unique_pointer() {
    let (a, b) = (malloc(0), malloc(0));
    assert_aligned(a);
    assert_aligned(b);
    assert_ne!(a, b);
    // SAFETY: each is a live pointer of `malloc`.
    unsafe { free(a) };
    // SAFETY: as above.
    unsafe { free(b) };
}

#[test]
fn each_block_is_aligned_and_holds_its_size() {
    for size in [1, 7, 16, 17, 100, 4096] {
        let ptr = malloc(size);
        assert_aligned(ptr);
        // SAFETY: the block holds `size` bytes.
        unsafe { ptr.cast::<u8>().write_bytes(0xa5, size) };
        // SAFETY: it is a live pointer of `malloc`.
        unsafe { free(ptr) };
    }
}

#[test]
fn a_failure_gives_null() {
    // Through a pointer, as C calls them: the optimizer may drop an inlined allocation
    // that is only compared with NULL, as if it succeeded.
    let (malloc, calloc): (
        extern "C" fn(usize) -> _,
        extern "C" fn(usize, usize) -> _,
    ) = std::hint::black_box((malloc, calloc));
    for size in [usize::MAX, usize::MAX - 15, isize::MAX.unsigned_abs(), HUGE] {
        assert!(malloc(size).is_null(), "malloc({size})");
        assert!(calloc(1, size).is_null(), "calloc(1, {size})");
    }
    assert!(calloc(usize::MAX, 2).is_null());
    assert!(calloc(1 << 33, 1 << 33).is_null());
}

#[test]
fn calloc_gives_zeroes() {
    let dirty = malloc(32);
    assert_aligned(dirty);
    // SAFETY: the block holds 32 bytes.
    unsafe { dirty.cast::<u8>().write_bytes(0xa5, 32) };
    // SAFETY: it is a live pointer of `malloc`. The next block of its size is likely
    // this one.
    unsafe { free(dirty) };
    let ptr = calloc(4, 8);
    assert_aligned(ptr);
    // SAFETY: the block holds 32 bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), 32) };
    assert_eq!(bytes, [0; 32]);
    // SAFETY: it is a live pointer of `calloc`.
    unsafe { free(ptr) };
}

#[test]
fn realloc_keeps_the_bytes() {
    // SAFETY: NULL is allowed.
    let ptr = unsafe { realloc(std::ptr::null_mut(), 8) };
    assert_aligned(ptr);
    // SAFETY: the block holds 8 bytes.
    unsafe { ptr.cast::<u8>().copy_from(b"abcdefgh".as_ptr(), 8) };
    let mut ptr = ptr;
    for size in [1000, 4] {
        // SAFETY: `ptr` is live.
        ptr = unsafe { realloc(ptr, size) };
        assert_aligned(ptr);
        // SAFETY: the block holds at least 4 bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), 4) };
        assert_eq!(bytes, b"abcd", "realloc to {size}");
    }
    // SAFETY: `ptr` is live.
    unsafe { free(ptr) };
}

#[test]
fn a_failed_realloc_keeps_the_block() {
    let ptr = malloc(4);
    // SAFETY: the block holds 4 bytes.
    unsafe { ptr.cast::<u8>().copy_from(b"abcd".as_ptr(), 4) };
    for size in [usize::MAX, HUGE] {
        // SAFETY: `ptr` is live.
        assert!(unsafe { realloc(ptr, size) }.is_null(), "realloc to {size}");
    }
    // SAFETY: the failed calls left the block live.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), 4) };
    assert_eq!(bytes, b"abcd");
    // SAFETY: as above.
    unsafe { free(ptr) };
}

#[test]
fn realloc_to_0_gives_a_pointer_as_malloc_of_0() {
    let ptr = malloc(64);
    // SAFETY: `ptr` is live.
    let empty = unsafe { realloc(ptr, 0) };
    assert_aligned(empty);
    // SAFETY: `empty` is live.
    unsafe { free(empty) };
}

#[test]
fn free_of_null_does_nothing() {
    // SAFETY: NULL is allowed.
    unsafe { free(std::ptr::null_mut()) };
}
