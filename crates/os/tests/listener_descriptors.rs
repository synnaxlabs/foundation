//! The first poll of a TCP listener with no free descriptor, which leaves it usable.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/descriptors.rs"]
mod descriptors;
#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only listens")]
mod sockets;

use std::task::{Context, Waker};

#[test]
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn a_first_poll_with_no_free_descriptor_leaves_the_listener_usable() {
    descriptors::limit();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    let _entered = runtime.enter();
    let mut listener = os::net().listen(&sockets::LISTEN).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let held = descriptors::take_each();
    assert!(listener.poll_accept(&mut cx).is_pending());
    drop(held);
    let client = std::net::TcpStream::connect(listener.local()).unwrap();
    let accepted =
        runtime.block_on(std::future::poll_fn(|cx| listener.poll_accept(cx)));
    assert_eq!(accepted.unwrap().peer(), client.local_addr().unwrap());
}
