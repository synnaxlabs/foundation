//! As the global allocator, `held` covers every thread. This binary has no test
//! harness, because a harness allocates on its own threads at any time.

#![cfg_attr(
    not(loom),
    expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")
)]

#[cfg(not(loom))]
use std::hint::black_box;

#[cfg(not(loom))]
#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// Loom's `Bytes` cannot be a `static`, and the loom model is in `bytes.rs`.
#[cfg(loom)]
fn main() {}

#[cfg(not(loom))]
fn main() {
    let before = ALLOCATOR.held();
    let mut held = black_box(vec![0_u8; 64]);
    assert_eq!(
        ALLOCATOR.held().strict_sub(before),
        64,
        "a vector holds its size"
    );
    held.reserve_exact(64);
    assert_eq!(
        ALLOCATOR.held().strict_sub(before),
        128,
        "a grown vector holds its size"
    );
    drop(held);
    assert_eq!(ALLOCATOR.held(), before, "a freed vector holds nothing");
}
