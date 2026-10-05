//! A library that lifts `disallowed_macros`, which the check refuses.

#![expect(
    clippy::disallowed_macros,
    reason = "a library never holds a global allocator"
)]
