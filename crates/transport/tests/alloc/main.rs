//! The tests that bound the allocations of transport with one count of them. The
//! count covers each thread, so this binary has no test harness. The sim runs on one
//! thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

mod chunks;
#[path = "../common/mod.rs"]
mod common;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    chunks::main();
}
