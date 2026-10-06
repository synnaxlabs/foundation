//! Production-path tests of `os` on the real operating system.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod common;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod files;
mod shards;
mod threads;
