//! A TCP listener that drops while a copy of its socket is open, as a child holds it
//! from its fork to its exec.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/copies.rs"]
mod copies;
#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only listens and connects")]
mod sockets;

use std::task::{Context, Waker};

use env::net::{Error, tcp};
use sockets::{LISTEN, OPTIONS};

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs pidfd_getfd and the shutdown of a listener"
)]
fn a_dropped_listener_refuses_while_a_copy_of_it_is_open() {
    let net = os::net();
    let mut listener = net.listen(&LISTEN).unwrap();
    let local = listener.local();
    let _copy = copies::copy_of(local);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    runtime.block_on(async {
        let mut cx = Context::from_waker(Waker::noop());
        assert!(listener.poll_accept(&mut cx).is_pending());
        drop(listener);
        let outcome = net
            .connect(&tcp::Config {
                remote: local,
                options: OPTIONS,
            })
            .await
            .map(|_| ());
        assert_eq!(outcome, Err(Error::Refused { remote: local }));
    });
}
