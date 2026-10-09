//! A child that the test spawns while `os` holds a socket of each kind. A child holds
//! each socket of this process that is not closed on exec, so this test runs in a test
//! binary of its own, with this one test only: the harness runs the tests of a binary
//! on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;
#[path = "common/sockets.rs"]
mod sockets;

#[test]
fn no_child_holds_a_socket_that_os_holds() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    let net = os::net();
    let held = runtime.block_on(async {
        let _sockets = sockets::open(&net).await;
        children::held()
    });
    assert_eq!(held, Vec::<String>::new());
}
