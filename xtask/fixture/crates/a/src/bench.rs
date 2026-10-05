//! A benchmark root in `src/`, which may declare a global allocator.

#[global_allocator]
static ALLOCATOR: std::alloc::System = std::alloc::System;

fn main() {}
