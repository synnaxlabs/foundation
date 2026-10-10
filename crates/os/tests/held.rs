//! A child that the test spawns while `os` holds a socket of each kind. A child holds
//! each socket of this process that is not closed on exec, so this test runs in a test
//! binary of its own, with this one test only: the harness runs the tests of a binary
//! on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
#[expect(dead_code, reason = "this binary starts no thread to work")]
mod children;
#[path = "common/sockets.rs"]
mod sockets;

use std::os::fd::AsRawFd;

use rustix::net::{AddressFamily, SocketType};

#[test]
fn no_child_holds_a_socket_that_os_holds() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    // Stands in for a socket that the parent of this process leaves open, as cargo
    // leaves the socket of a download.
    let _left_open = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None)
        .expect("a UDP socket opens");
    let baseline = children::Baseline::list();
    let opened = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None)
        .expect("a UDP socket opens");
    let path = format!("/dev/fd/{}", opened.as_raw_fd());
    assert_eq!(
        baseline.added(),
        vec![path],
        "a socket opened after the baseline"
    );
    drop(opened);
    let net = os::net();
    let held = runtime.block_on(async {
        let _sockets = sockets::open(&net).await;
        baseline.added()
    });
    assert_eq!(held, Vec::<String>::new());
}
