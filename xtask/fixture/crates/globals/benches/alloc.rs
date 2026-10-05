//! A benchmark binary that lifts `disallowed_macros`, which the check allows.

#![expect(clippy::disallowed_macros, reason = "the counting allocator")]

static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn main() {}
