//! A TCP listener that drops while a copy of its socket is open, as a child holds it
//! from its fork to its exec. Each test takes a copy of each descriptor of the process
//! for a moment, so the tests run in a test binary of their own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only listens and connects")]
mod sockets;

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::task::{Context, Poll, Waker};

use env::net::{Error, tcp};
use rustix::io::Errno;
use sockets::{LISTEN, OPTIONS};

fn connect_config(remote: SocketAddr) -> tcp::Config {
    tcp::Config {
        remote,
        options: OPTIONS,
    }
}

/// A current-thread runtime with an I/O driver, for a poll on the test thread.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds")
}

/// A copy of the socket bound to `local`, as a child holds it from its fork to its exec.
fn copy_of(local: SocketAddr) -> OwnedFd {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{PidfdFlags, PidfdGetfdFlags, getpid};
        use rustix::process::{pidfd_getfd, pidfd_open};

        let pidfd = pidfd_open(getpid(), PidfdFlags::empty()).unwrap();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let name = entry.unwrap().file_name();
            let fd = name
                .to_string_lossy()
                .parse()
                .expect("a descriptor is a number");
            let copy = match pidfd_getfd(&pidfd, fd, PidfdGetfdFlags::empty()) {
                Ok(copy) => copy,
                // Another test, or the read of the directory, closed it since.
                Err(Errno::BADF) => continue,
                Err(e) => panic!("a copy of descriptor {fd}: {e:?}"),
            };
            let name = rustix::net::getsockname(&copy).ok();
            if name.and_then(|n| SocketAddr::try_from(n).ok()) == Some(local) {
                return copy;
            }
        }
    }
    panic!("no descriptor is bound to {local}");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs pidfd_getfd and the shutdown of a listener"
)]
fn a_dropped_listener_refuses_while_a_copy_of_it_is_open() {
    let net = os::net();
    let mut listener = net.listen(&LISTEN).unwrap();
    let local = listener.local();
    let _copy = copy_of(local);
    let runtime = runtime();
    runtime.block_on(async {
        let mut cx = Context::from_waker(Waker::noop());
        assert!(listener.poll_accept(&mut cx).is_pending());
        drop(listener);
        let outcome = net.connect(&connect_config(local)).await.map(|_| ());
        assert_eq!(outcome, Err(Error::Refused { remote: local }));
    });
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
    let _copy = copy_of(local);
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
    let outcome = runtime().block_on(net.connect(&connect_config(local)));
    assert_eq!(outcome.map(|_| ()), Err(Error::Refused { remote: local }));
}
