//! Children that the test spawns while other threads open sockets. A child holds each
//! socket of this process that is not closed on exec, so this test runs in a test
//! binary of its own, with this one test only: the harness runs the tests of a binary
//! on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use children::{BIND, LISTEN, held};

const OPENERS: usize = 4;
const CHILDREN: usize = 500;

/// Binds a UDP socket and listens on TCP, on loopback, and drops each, until `stop`.
fn open(stop: &AtomicBool) {
    let net = os::net();
    while !stop.load(Ordering::Relaxed) {
        drop(net.udp(&BIND).expect("bind on loopback"));
        drop(net.listen(&LISTEN).expect("listen on loopback"));
    }
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs SOCK_CLOEXEC")]
fn no_child_holds_a_socket_that_another_thread_opens() {
    let threads = os::threads().expect("the OS gives the cores of this process");
    let stop = Arc::new(AtomicBool::new(false));
    let openers: Vec<_> = (0..OPENERS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let opener = threads.start("open", move || async move { open(&stop) });
            opener.expect("the thread starts")
        })
        .collect();
    let held: Vec<_> = (0..CHILDREN).flat_map(|_| held()).collect();
    stop.store(true, Ordering::Relaxed);
    for opener in openers {
        opener.join().expect("the opener runs with no panic");
    }
    assert_eq!(held, Vec::<String>::new());
}
