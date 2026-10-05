//! A test target, which may declare a global allocator.

#[global_allocator]
static ALLOCATOR: std::alloc::System = std::alloc::System;
