//! The descriptors of the process, for the test binaries that take each one. Each
//! such binary holds one test only: the harness runs the tests of a binary on threads
//! of one process.

use std::os::fd::OwnedFd;

use rustix::fs::{Mode, OFlags, open};
use rustix::process::{self, Resource};

/// Sets the limit of descriptors of the process to 128. A small limit runs out before
/// the table of the host does, so a call gives `EMFILE`, not `ENFILE`.
pub(crate) fn limit() {
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(128);
    process::setrlimit(Resource::Nofile, limit).unwrap();
}

/// Takes each free descriptor of the process until the result drops.
pub(crate) fn take_each() -> Vec<OwnedFd> {
    let mut held = Vec::new();
    while let Ok(fd) = open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        held.push(fd);
    }
    held
}
