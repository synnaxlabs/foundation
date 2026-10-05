//! Defines the injected seams for monotonic time, the OS wall clock (read only by
//! `clock`), randomness, shards, dedicated threads, and task spawning. Files come
//! later.
//!
//! Each seam is a concrete handle over a small driver trait. Only `os` and `sim`
//! implement the drivers: `os` on the real operating system, `sim` in a deterministic
//! simulation. `node` builds the real handles and passes them down; nothing else
//! reaches the operating system.

pub mod clock;
pub mod entropy;
pub mod rng;
pub mod shards;
pub mod tasks;
pub mod threads;
pub mod wall;
