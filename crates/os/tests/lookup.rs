//! Children that the test spawns after one lookup of a host name, and while another
//! thread looks up a host name. A child holds each socket of this process that is not
//! closed on exec, so this test runs in a test binary of its own, with this one test
//! only: the harness runs the tests of a binary on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs glibc")]
fn no_child_holds_a_socket_of_a_lookup() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    // Ends before the first child, for a socket that the C library keeps open.
    let found = runtime.block_on(os::net().resolve("localhost", 80));
    found.expect("localhost has an address");
    let held = children::held_while(1, async |net| {
        let found = net.resolve("localhost", 80).await;
        found.expect("localhost has an address");
    });
    assert_eq!(held, Vec::<String>::new());
}
