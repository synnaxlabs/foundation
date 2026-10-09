//! An accept with no free descriptor, which leaves the listener usable.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/descriptors.rs"]
mod descriptors;
#[path = "common/sockets.rs"]
#[expect(dead_code, reason = "this binary only listens")]
mod sockets;

use std::future::poll_fn;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use env::net::{Error, tcp};
use rustix::io::Errno;

#[test]
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn an_accept_with_no_free_descriptor_is_emfile_and_leaves_the_listener_usable() {
    descriptors::limit();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds");
    let _entered = runtime.enter();
    let config = tcp::Listen {
        backlog: 4,
        ..sockets::LISTEN
    };
    let mut listener = os::net().listen(&config).unwrap();
    let first = std::net::TcpStream::connect(listener.local()).unwrap();
    let second = std::net::TcpStream::connect(listener.local()).unwrap();
    let third = std::net::TcpStream::connect(listener.local()).unwrap();
    // The first accept makes the listener ready, and a success keeps it so.
    let accepted = runtime.block_on(poll_fn(|cx| listener.poll_accept(cx)));
    assert_eq!(accepted.unwrap().peer(), first.local_addr().unwrap());
    let held = descriptors::take_each();
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
        third
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
