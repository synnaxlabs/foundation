//! Children that the test spawns while other threads open sockets. A socket that
//! another test opens with no close-on-exec flag would reach a child, so it runs in a
//! test binary of its own, with this one test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env::net::tcp::{Listen, Options};
use env::net::udp;

const OPENERS: usize = 4;
const CHILDREN: usize = 500;

/// Binds a UDP socket and listens on TCP, on loopback, and drops each, until `stop`.
fn open(stop: &AtomicBool) {
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let bind = udp::Config {
        local,
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    let listen = Listen {
        local,
        backlog: 1,
        options: Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
            delayed: false,
        },
    };
    let net = os::net();
    while !stop.load(Ordering::Relaxed) {
        drop(net.udp(&bind).expect("bind on loopback"));
        drop(net.listen(&listen).expect("listen on loopback"));
    }
}

/// Each socket that a new child holds after its exec, as `ls` lists it. `-n` looks up
/// no user, which may open a socket in the child.
fn held() -> Vec<String> {
    let listed = Command::new("ls")
        .args(["-ln", "/proc/self/fd"])
        .output()
        .expect("run ls");
    assert!(listed.status.success(), "ls lists the descriptors");
    let listed = String::from_utf8(listed.stdout).expect("ls writes UTF-8");
    let sockets = listed.lines().filter(|line| line.contains("socket:"));
    sockets.map(String::from).collect()
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs SOCK_CLOEXEC")]
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
