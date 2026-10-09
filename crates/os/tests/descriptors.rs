//! A name lookup with no free descriptor. glibc loads its name service modules at the
//! first lookup of a process, and gives `EAI_NONAME` when it cannot.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[path = "common/descriptors.rs"]
mod descriptors;

use env::net::Error;
use rustix::io::Errno;
use tokio::runtime::Runtime;

/// Looks up `localhost` while each descriptor of the process is taken.
fn starved(runtime: &Runtime) -> Result<Vec<std::net::SocketAddr>, Error> {
    let _held = descriptors::take_each();
    runtime.block_on(os::net().resolve("localhost", 4433))
}

#[test]
fn a_lookup_with_no_free_descriptor_is_io() {
    descriptors::limit();
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
