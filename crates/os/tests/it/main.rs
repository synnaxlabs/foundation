//! Production-path tests of `os` on the real operating system.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod clock;
mod common;
mod entropy;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod files;
#[cfg(target_os = "linux")]
mod kept;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod net;
mod shards;
mod threads;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod wall;
