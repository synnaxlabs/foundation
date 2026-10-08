//! C allocates and frees through the global allocator of the binary, each allocation
//! function writes the size into the header that `free` reads, and a drop frees each
//! block that C holds. As the global allocator, `held` covers every thread, so this
//! binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]
#![expect(unsafe_code, reason = "the test calls the C library")]

use std::ffi::c_void;

use connector_opcua::bench::Client;
use sim::Sim;

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
    the_drop_frees_the_client_its_loop_and_its_timers();
    each_function_writes_the_size_into_the_header();
    the_fuzz_round_trip_frees_each_value();
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

fn the_drop_frees_the_client_its_loop_and_its_timers() {
    let mut sim = Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    let before = ALLOCATOR.held();
    let client = Client::new(env::clock::Clock::clone(&clock), 100);
    let held = ALLOCATOR.held().strict_sub(before);
    assert!(
        held > 100 * size_of::<usize>(),
        "{held} bytes for 100 timers"
    );
    drop(client);
    assert_eq!(ALLOCATOR.held(), before, "the drop freed each block");
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
    // SAFETY: `block` is live and from the allocator.
    let block = unsafe { connector_opcua_realloc(block, 0) };
    assert!(!block.is_null(), "realloc to 0 gave a block");
    assert_eq!(held(), HEADER, "realloc to 0 freed the old block");
    // SAFETY: `block` is live and from the allocator, and freed once.
    unsafe { connector_opcua_free(block) };
    assert_eq!(ALLOCATOR.held(), before, "free gave back what realloc held");
}

fn the_fuzz_round_trip_frees_each_value() {
    // A `Variant` (23, as `ffi` is private) of 7 `ExtensionObject` values, then the
    // zeros that #435 needs.
    let mut data = vec![23, 0, 0x96, 7, 0, 0, 0];
    data.resize(data.len() + 7 * 4, 0);
    let before = ALLOCATOR.held();
    connector_opcua::fuzz::decode(&data);
    assert_eq!(ALLOCATOR.held(), before, "the round trip freed each block");
}
