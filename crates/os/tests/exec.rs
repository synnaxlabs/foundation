//! Children that the tests spawn while `os` holds sockets. A socket that one test opens
//! with no close-on-exec flag would reach the child of another, so these tests run in a
//! test binary of their own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env::net::tcp::{self, Listen, Options};
use env::net::udp::{self, Meta, Transmit};

const OPENERS: usize = 4;
const CHILDREN: usize = 500;

const LOCAL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

const BIND: udp::Config = udp::Config {
    local: LOCAL,
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
};

const OPTIONS: Options = Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
    delayed: false,
};

const LISTEN: Listen = Listen {
    local: LOCAL,
    backlog: 1,
    options: OPTIONS,
};

/// Binds a UDP socket and listens on TCP, on loopback, and drops each, until `stop`.
fn open(stop: &AtomicBool) {
    let net = os::net();
    while !stop.load(Ordering::Relaxed) {
        drop(net.udp(&BIND).expect("bind on loopback"));
        drop(net.listen(&LISTEN).expect("listen on loopback"));
    }
}

/// Each socket that a new child holds after its exec, by its path in `/dev/fd`.
fn held() -> Vec<String> {
    let list = r#"for f in /dev/fd/*; do if [ -S "$f" ]; then echo "$f"; fi; done"#;
    let listed = (Command::new("sh").args(["-c", list]).output()).expect("run sh");
    assert!(listed.status.success(), "sh lists the descriptors");
    let listed = String::from_utf8(listed.stdout).expect("sh writes UTF-8");
    listed.lines().map(String::from).collect()
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "macOS opens a socket in two calls")]
fn no_child_holds_a_socket_that_another_thread_opens() {
    let threads = os::threads().expect("the OS gives the cores of this process");
    let stop = Arc::new(AtomicBool::new(false));
    let openers: Vec<_> = (0..OPENERS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let opener = threads.start("open", move || async move { open(&stop) });
            opener.expect("the thread starts")
        })
        .collect();
    let held: Vec<_> = (0..CHILDREN).flat_map(|_| held()).collect();
    stop.store(true, Ordering::Relaxed);
    for opener in openers {
        opener.join().expect("the opener runs with no panic");
    }
    assert_eq!(held, Vec::<String>::new());
}

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
