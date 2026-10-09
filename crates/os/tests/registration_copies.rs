//! A TCP listener whose registration fails while a copy of its socket is open, as a
//! child holds it from its fork to its exec.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/copies.rs"]
mod copies;
#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only listens and connects")]
mod sockets;

use std::task::{Context, Poll, Waker};

use env::net::{Error, tcp};
use rustix::io::Errno;
use sockets::{LISTEN, OPTIONS};

/// A current-thread runtime with an I/O driver.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds")
}

/// The runtime of the thread shut down, so the registration fails.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs pidfd_getfd and the shutdown of a listener"
)]
fn a_listener_with_a_failed_registration_refuses_while_a_copy_is_open() {
    let net = os::net();
    let mut listener = net.listen(&LISTEN).unwrap();
    let local = listener.local();
    let _copy = copies::copy_of(local);
    let handle = runtime().handle().clone();
    let entered = handle.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let Poll::Ready(Err(failed)) = listener.poll_accept(&mut cx) else {
        panic!("the registration fails");
    };
    assert_eq!(
        failed,
        Error::Io {
            code: Errno::IO.raw_os_error()
        }
    );
    drop((entered, listener));
    let outcome = runtime().block_on(net.connect(&tcp::Config {
        remote: local,
        options: OPTIONS,
    }));
    assert_eq!(outcome.map(|_| ()), Err(Error::Refused { remote: local }));
}
