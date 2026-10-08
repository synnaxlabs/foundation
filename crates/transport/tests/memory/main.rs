//! The tests that bound the heap of transport with one count of the bytes it holds.
//! The count covers each thread, so this binary has no test harness. The sim runs on
//! one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/mod.rs"]
mod common;
mod held;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

fn main() {
    held::main();
}
