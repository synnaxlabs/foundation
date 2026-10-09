//! Children that the test spawns while another thread looks up a host name. A child
//! holds each socket of this process that is not closed on exec, so this test runs in
//! a test binary of its own, with this one test only: the harness runs the tests of a
//! binary on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use children::held;

const CHILDREN: usize = 500;

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs glibc")]
fn no_child_holds_a_socket_of_a_lookup() {
    let threads = os::threads().expect("the OS gives the cores of this process");
    let stop = Arc::new(AtomicBool::new(false));
    let looker = {
        let stop = Arc::clone(&stop);
        threads.start("look", move || async move {
            let net = os::net();
            while !stop.load(Ordering::Relaxed) {
                let found = net.resolve("localhost", 80).await;
                found.expect("localhost has an address");
            }
        })
    };
    let looker = looker.expect("the thread starts");
    let held: Vec<_> = (0..CHILDREN).flat_map(|_| held()).collect();
    stop.store(true, Ordering::Relaxed);
    looker.join().expect("the lookups run with no panic");
    assert_eq!(held, Vec::<String>::new());
}
