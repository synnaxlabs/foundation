//! A test binary that lifts `disallowed_macros` and holds a global allocator, which the
//! check allows, and another `static`, which it refuses.

#![expect(clippy::disallowed_macros, reason = "the counting allocator")]

#[global_allocator]
/// Comments may come between the attribute and the item.
static ALLOCATOR: std::alloc::System = std::alloc::System;

static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
