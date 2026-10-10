//! Children that the test spawns while other threads open sockets. A child holds each
//! socket of this process that is not closed on exec, so this test runs in a test
//! binary of its own, with this one test only: the harness runs the tests of a binary
//! on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;
#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only binds and listens")]
mod sockets;

use sockets::{BIND, LISTEN};

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs SOCK_CLOEXEC")]
fn no_child_holds_a_socket_that_another_thread_opens() {
    let baseline = children::Baseline::list();
    let held = children::held_while(&baseline, 4, async |net| {
        drop(net.udp(&BIND).expect("bind on loopback"));
        drop(net.listen(&LISTEN).expect("listen on loopback"));
    });
    assert_eq!(held, Vec::<String>::new());
}
