//! Only Unix: `os` has no `net` driver on Windows.
//!
//! A `[::]` bind on port 0 with one free descriptor, which the probe for a free port
//! cannot also take. It sets the limit of the process, so it runs in a test binary of
//! its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/descriptors.rs"]
mod descriptors;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use env::net::udp;

fn config(local: SocketAddr) -> udp::Config {
    udp::Config {
        local,
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    }
}

/// An IPv4 bind needs one descriptor, and so does a `[::]` bind (#3422).
#[test]
fn a_bind_on_any_v6_port_0_with_one_free_descriptor_binds() {
    descriptors::limit();
    let net = os::net();
    let mut held = descriptors::take_each();
    drop(held.pop());
    let v4 = net.udp(&config(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)));
    assert!(v4.is_ok(), "{:?}", v4.err());
    drop(held.pop());
    let v6 = net.udp(&config(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)));
    assert!(v6.is_ok(), "{:?}", v6.err());
}
