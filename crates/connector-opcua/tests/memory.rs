//! C allocates and frees through the global allocator of the binary, each allocation
//! function writes the size into the header that `free` reads, and a drop frees each
//! block that C holds. As the global allocator, `held` covers every thread, so this
//! binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]
#![expect(unsafe_code, reason = "the test calls the C library")]

use std::ffi::c_void;

use connector_opcua::bench::{
    Bytes, Client, client, connector_opcua_free, connector_opcua_malloc,
    connector_opcua_realloc,
};
use sim::Sim;
use types::sample::Scalar;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// A `UA_NodeId` with a numeric identifier.
#[repr(C, align(8))]
struct Key {
    namespace: u16,
    kind: u32,
    numeric: u32,
    rest: [u32; 3],
}

unsafe extern "C" {
    fn UA_findDataType(key: *const Key) -> *const c_void;
    fn UA_ByteString_allocBuffer(bytes: *mut Bytes, length: usize) -> u32;
    fn UA_clear(value: *mut c_void, kind: *const c_void);
}

/// The size of the header before each block of the allocator.
const HEADER: usize = 16;

fn main() {
    c_allocates_through_the_global_allocator();
    the_drop_frees_the_client_its_loop_and_its_timers();
    each_function_writes_the_size_into_the_header();
    the_fuzz_round_trip_frees_each_value();
    a_client_frees_what_it_holds();
    a_describe_frees_its_requests_and_answers();
}

fn c_allocates_through_the_global_allocator() {
    let key = Key {
        namespace: 0,
        kind: 0,
        // The type ByteString.
        numeric: 15,
        rest: [0; 3],
    };
    // SAFETY: `key` is a valid numeric node key.
    let kind = unsafe { UA_findDataType(&raw const key) };
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
    let node = sim.node(sim::node::Config::default());
    let before = ALLOCATOR.held();
    let client = Client::new(node.clock(), &mut node.entropy().rng(), 100);
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
    let block = connector_opcua_malloc(10);
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

/// Each cycle connects a client, reads, closes the client, and drops it. The first
/// cycle also grows the memory of the sim and of the server, so the test compares the
/// later cycles. A client that only drops leaves its session on the server until it
/// times out, which the server holds.
fn a_client_frees_what_it_holds() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let run = move |node: sim::node::Node, _| async move {
        let address = node.addresses()[0];
        let body = async |server: &client::Server| {
            let mut held = Vec::new();
            for _ in 0..4 {
                let mut client = server.connect(64).await;
                client.read(client::Variable::Number).await;
                client.close().await;
                held.push(ALLOCATOR.held());
            }
            held
        };
        let rng = node.entropy().rng();
        client::Server::scope(node.clock(), node.net(), address, rng, body).await
    };
    let held = sim.run_on(&node, run).expect("the run ends");
    assert_eq!(
        held[2..],
        [held[1]; 2],
        "each held byte count after the first"
    );
}

fn a_describe_frees_its_requests_and_answers() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let run = move |node: sim::node::Node, _| async move {
        let address = node.addresses()[0];
        let body = async |server: &client::Server| {
            let mut client = server.connect(1).await;
            let mut held = Vec::new();
            let mut bases = Vec::new();
            for _ in 0..4 {
                bases.push(client.describe().await);
                held.push(ALLOCATOR.held());
            }
            client.close().await;
            (bases, held)
        };
        let rng = node.entropy().rng();
        client::Server::scope(node.clock(), node.net(), address, rng, body).await
    };
    let (bases, held) = sim.run_on(&node, run).expect("the run ends");
    assert_eq!(bases, [Some(Scalar::F64); 4], "each base is Double");
    assert_eq!(
        held[2..],
        [held[1]; 2],
        "each held byte count after the first"
    );
}
