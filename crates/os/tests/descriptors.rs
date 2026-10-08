//! A name lookup with no free descriptor. It takes each descriptor of the process, so
//! it runs in a test binary of its own, with this one test only: the harness runs the
//! tests of a binary on threads of one process. glibc loads its name service modules
//! at the first lookup of a process, and gives `EAI_NONAME` when it cannot.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use env::net::Error;
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{self, Resource};
use tokio::runtime::Runtime;

/// Looks up `localhost` while each descriptor of the process is taken.
fn starved(runtime: &Runtime) -> Result<Vec<std::net::SocketAddr>, Error> {
    let mut held = Vec::new();
    while let Ok(fd) = open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        held.push(fd);
    }
    runtime.block_on(os::net().resolve("localhost", 4433))
}

#[test]
fn a_lookup_with_no_free_descriptor_is_io() {
    // A small limit runs out before the table of the host does, which gives `ENFILE`.
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(128);
    process::setrlimit(Resource::Nofile, limit).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    let io = Err(Error::Io {
        code: Errno::MFILE.raw_os_error(),
    });
    assert_eq!(starved(&runtime), io, "at the first lookup");
    let found = runtime.block_on(os::net().resolve("localhost", 4433));
    assert!(found.is_ok_and(|found| !found.is_empty()));
    assert_eq!(starved(&runtime), io, "with the modules loaded");
}
