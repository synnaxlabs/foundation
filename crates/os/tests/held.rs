//! A child that the test spawns while `os` holds a socket of each kind. A child holds
//! each socket of this process that is not closed on exec, so this test runs in a test
//! binary of its own, with this one test only: the harness runs the tests of a binary
//! on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/children.rs"]
mod children;

use std::future::poll_fn;
use std::io::IoSliceMut;

use children::{BIND, LISTEN, OPTIONS, held};
use env::net::tcp;
use env::net::udp::{Meta, Transmit};

/// A listener, a stream that `os` connects and one that it accepts, and a UDP socket
/// whose halves each sent or received once, so each holds a descriptor of its own.
#[test]
fn no_child_holds_a_socket_that_os_holds() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    let net = os::net();
    let held = runtime.block_on(async {
        let mut listener = net.listen(&LISTEN).expect("listen on loopback");
        let connect = tcp::Config {
            remote: listener.local(),
            options: OPTIONS,
        };
        let _client = net.connect(&connect).await.expect("the listener accepts");
        let accepted = poll_fn(|cx| listener.poll_accept(cx)).await;
        let _server = accepted.expect("a stream waits in the backlog");
        let (mut sender, mut receiver) = net.udp(&BIND).expect("bind on loopback");
        let transmit = Transmit {
            destination: receiver.local(),
            source: None,
            ecn: None,
            contents: b"x",
            segment: None,
        };
        let sent = poll_fn(|cx| sender.poll_send(cx, &transmit)).await;
        sent.expect("the send");
        let mut buffer = [0; 8];
        let mut meta = [Meta::default()];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let arrived = poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta));
        assert_eq!(arrived.await, Ok(1));
        held()
    });
    assert_eq!(held, Vec::<String>::new());
}
