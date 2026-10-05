//! Defines the injected seams for monotonic time, the OS wall clock (read only by
//! `clock`), files, randomness, threads, and task spawning.
//!
//! Each seam is a concrete handle over a small driver trait. `os` implements the
//! drivers on the real operating system and `sim` implements them in a deterministic
//! simulation. `node` builds the real handles and passes them down; nothing else
//! reaches the operating system.

pub mod clock;
pub mod entropy;
pub mod rng;
pub mod tasks;
pub mod threads;
pub mod wall;

pub use clock::Clock;
pub use entropy::Entropy;
pub use rng::Rng;
pub use tasks::Tasks;
pub use threads::Threads;
pub use wall::Wall;
