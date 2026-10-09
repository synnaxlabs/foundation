//! An accept with no free descriptor, which leaves the listener usable. It takes each
//! descriptor of the process, so it runs in a test binary of its own, with this one
//! test only: the harness runs the tests of a binary on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::future::poll_fn;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use env::net::{Error, tcp};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{self, Resource};

#[test]
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn an_accept_with_no_free_descriptor_is_emfile_and_leaves_the_listener_usable() {
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(128);
    process::setrlimit(Resource::Nofile, limit).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds");
    let _entered = runtime.enter();
    let config = tcp::Listen {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        backlog: 4,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
            delayed: false,
        },
    };
    let mut listener = os::net().listen(&config).unwrap();
    let first = std::net::TcpStream::connect(listener.local()).unwrap();
    let second = std::net::TcpStream::connect(listener.local()).unwrap();
    // The first accept makes the listener ready, and a success keeps it so.
    let accepted = runtime.block_on(poll_fn(|cx| listener.poll_accept(cx)));
    assert_eq!(accepted.unwrap().peer(), first.local_addr().unwrap());
    let mut held = Vec::new();
    while let Ok(fd) = open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        held.push(fd);
    }
    let mut cx = Context::from_waker(Waker::noop());
    let Poll::Ready(Err(failed)) = listener.poll_accept(&mut cx) else {
        panic!("the accept fails with no free descriptor");
    };
    assert_eq!(
        failed,
        Error::Io {
            code: Errno::MFILE.raw_os_error()
        }
    );
    drop(held);
    // macOS closes the connection that the failed accept took off the queue.
    let next = if cfg!(target_os = "macos") {
        std::net::TcpStream::connect(listener.local()).unwrap()
    } else {
        second
    };
    let accepted = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            poll_fn(|cx| listener.poll_accept(cx)),
        )
        .await
    });
    let accepted = accepted.expect("the listener stays usable").unwrap();
    assert_eq!(accepted.peer(), next.local_addr().unwrap());
}
