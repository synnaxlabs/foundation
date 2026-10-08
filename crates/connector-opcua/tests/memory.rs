//! C allocates and frees through the global allocator of the binary, and each
//! allocation function writes the size into the header that `free` reads. As the
//! global allocator, `held` covers every thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]
#![expect(unsafe_code, reason = "the test calls the C library")]

use std::ffi::c_void;

use connector_opcua as _;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// A `UA_ByteString`.
#[repr(C)]
struct Bytes {
    length: usize,
    data: *mut u8,
}

/// A `UA_NodeId` with a numeric identifier.
#[repr(C, align(8))]
struct NodeId {
    namespace: u16,
    kind: u32,
    numeric: u32,
    rest: [u32; 3],
}

unsafe extern "C" {
    fn UA_findDataType(id: *const NodeId) -> *const c_void;
    fn UA_ByteString_allocBuffer(bytes: *mut Bytes, length: usize) -> u32;
    fn UA_clear(value: *mut c_void, kind: *const c_void);
    fn connector_opcua_malloc(size: usize) -> *mut c_void;
    fn connector_opcua_realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn connector_opcua_free(ptr: *mut c_void);
}

/// The size of the header before each block of the allocator.
const HEADER: usize = 16;

fn main() {
    c_allocates_through_the_global_allocator();
    each_function_writes_the_size_into_the_header();
}

fn c_allocates_through_the_global_allocator() {
    let id = NodeId {
        namespace: 0,
        kind: 0,
        // The type ByteString.
        numeric: 15,
        rest: [0; 3],
    };
    // SAFETY: `id` is a valid numeric node key.
    let kind = unsafe { UA_findDataType(&raw const id) };
    assert!(!kind.is_null(), "the copy holds the type ByteString");
    let before = ALLOCATOR.held();
    let mut bytes = Bytes {
        length: 0,
        data: std::ptr::null_mut(),
    };
    // SAFETY: `bytes` is a valid byte string.
    let status = unsafe { UA_ByteString_allocBuffer(&raw mut bytes, 100) };
    assert_eq!(status, 0, "the allocation succeeded");
    assert_eq!(
        ALLOCATOR.held().strict_sub(before),
        100 + HEADER,
        "C allocated through the global allocator"
    );
    // SAFETY: `kind` is the type of `bytes`.
    unsafe { UA_clear((&raw mut bytes).cast(), kind) };
    assert_eq!(
        ALLOCATOR.held(),
        before,
        "C freed through the global allocator"
    );
}

/// `calloc` is the path above; this pins the header that `malloc` and `realloc` write,
/// which `free` reads to give the block back.
fn each_function_writes_the_size_into_the_header() {
    let before = ALLOCATOR.held();
    let held = || ALLOCATOR.held().strict_sub(before);
    // SAFETY: any size is valid.
    let block = unsafe { connector_opcua_malloc(10) };
    assert!(!block.is_null(), "malloc gave a block");
    assert_eq!(held(), 10 + HEADER, "malloc");
    // SAFETY: `block` is live and from the allocator.
    let block = unsafe { connector_opcua_realloc(block, 50) };
    assert!(!block.is_null(), "realloc gave a block");
    assert_eq!(held(), 50 + HEADER, "realloc");
    // SAFETY: `block` is live and from the allocator.
    let block = unsafe { connector_opcua_realloc(block, 20) };
    assert_eq!(held(), 20 + HEADER, "realloc down");
    // SAFETY: `block` is live and from the allocator, and freed once.
    unsafe { connector_opcua_free(block) };
    assert_eq!(ALLOCATOR.held(), before, "free gave back what realloc held");
}
