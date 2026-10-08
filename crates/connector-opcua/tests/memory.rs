//! C allocates and frees through the global allocator of the binary, and a drop frees
//! each block that C holds. As the global allocator, `held` covers every thread, so
//! this binary has no test harness.

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
}

/// The size of the header before each block of the allocator.
const HEADER: usize = 16;

fn main() {
    c_allocates_through_the_global_allocator();
    the_drop_frees_the_client_its_loop_and_its_timers();
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
