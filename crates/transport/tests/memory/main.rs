//! The tests that bound the heap of transport with one count of the bytes it holds.
//! The count covers each thread, so this binary has no test harness. The sim runs on
//! one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/mod.rs"]
mod common;
mod held;

use block::{Pool, Unique};

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

fn main() {
    held::main();
}

/// Takes every block of `pool` that could hold a message of `len` bytes.
fn fill(pool: &Pool, len: usize) -> Vec<Unique> {
    let mut full = Vec::new();
    for len in [pool.largest(), len] {
        while let Ok(block) = pool.alloc(len) {
            full.push(block);
        }
    }
    full
}
