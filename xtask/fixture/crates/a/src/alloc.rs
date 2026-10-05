//! A file that no target compiles, with a global allocator.

/// Declared with `#[global_allocator]`.
#[global_allocator]
static ALLOCATOR: std::alloc::System = std::alloc::System;
