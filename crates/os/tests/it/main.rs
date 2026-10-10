//! Production-path tests of `os` on the real operating system.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod clock;
mod common;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "../common/mod.rs"]
mod disk;
mod entropy;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod files;
#[cfg(target_os = "linux")]
mod kept;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod net;
#[cfg(target_os = "linux")]
#[path = "../common/seccomp.rs"]
mod seccomp;
mod shards;
mod threads;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod wall;
