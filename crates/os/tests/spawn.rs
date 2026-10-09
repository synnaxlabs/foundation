//! A lookup whose thread cannot start. The filter acts on the test thread and each
//! thread it starts, so it runs in a test binary of its own, with this one test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[path = "common/threads.rs"]
mod threads;

use env::net::Error;
use rustix::io::Errno;

#[test]
fn a_lookup_whose_thread_cannot_start_is_io() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    threads::refuse();
    let found = runtime.block_on(os::net().resolve("localhost", 4433));
    let code = Errno::AGAIN.raw_os_error();
    assert_eq!(found, Err(Error::Io { code }));
}
