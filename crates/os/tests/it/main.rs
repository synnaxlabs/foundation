//! Production-path tests of `os` on the real operating system.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod shards;
mod threads;
