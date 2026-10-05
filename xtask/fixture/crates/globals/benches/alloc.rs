//! A benchmark binary that lifts `disallowed_macros`, which the check allows.

#![expect(clippy::disallowed_macros, reason = "the counting allocator")]

fn main() {}
