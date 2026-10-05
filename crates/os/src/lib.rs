//! Implements the `env` seams on the real operating system: monotonic and wall clocks,
//! files, randomness, and threads, and the memory of block pools. The only crate
//! allowed to call them.

#[expect(unsafe_code, reason = "a pool's memory is an OS mapping")]
pub mod memory;
