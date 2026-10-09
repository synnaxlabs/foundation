//! The first poll of a TCP listener with no free descriptor, which leaves it usable. It
//! takes each descriptor of the process, so it runs in a test binary of its own, with
//! this one test only: the harness runs the tests of a binary on threads of one
//! process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};

use env::net::tcp;
use rustix::fs::{Mode, OFlags, open};
use rustix::process::{self, Resource};

#[test]
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn a_first_poll_with_no_free_descriptor_leaves_the_listener_usable() {
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(128);
    process::setrlimit(Resource::Nofile, limit).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    let _entered = runtime.enter();
    let config = tcp::Listen {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        backlog: 1,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
            delayed: false,
        },
    };
    let mut listener = os::net().listen(&config).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let mut held = Vec::new();
    while let Ok(fd) = open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        held.push(fd);
    }
    assert!(listener.poll_accept(&mut cx).is_pending());
    drop(held);
    let _client = std::net::TcpStream::connect(listener.local()).unwrap();
    let accepted =
        runtime.block_on(std::future::poll_fn(|cx| listener.poll_accept(cx)));
    assert!(accepted.is_ok());
}
