//! Only Unix: `os` has no `net` driver on Windows.
//!
//! A `[::]` bind on port 0 while IPv6 sockets hold most free ports and IPv4 sockets
//! hold none. It holds thousands of descriptors, so it runs in a test binary of its
//! own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::net::{Ipv6Addr, SocketAddr};

use env::net::udp;
use rustix::process::{self, Resource};

/// The first and last port of the range from which this host picks the port of a
/// bind to port 0.
fn range() -> (u32, u32) {
    #[cfg(target_os = "linux")]
    let text =
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    #[cfg(target_os = "macos")]
    let text = ["first", "last"]
        .map(|end| {
            let output = std::process::Command::new("sysctl")
                .args(["-n", &format!("net.inet.ip.portrange.{end}")])
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        })
        .join(" ");
    let mut ports = text.split_whitespace().map(|p| p.parse().unwrap());
    (ports.next().unwrap(), ports.next().unwrap())
}

fn config(local: SocketAddr) -> udp::Config {
    udp::Config {
        local,
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    }
}

/// The IPv4 probe sees none of the held ports, so each of its tries fails with a
/// chance of 0.6, and all of them fail for about 1 bind in 60 (#3422).
#[test]
fn port_0_on_any_v6_binds_while_a_free_port_is_left() {
    let (first, last) = range();
    let count = (last - first + 1) * 6 / 10;
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(u64::from(count) + 256);
    process::setrlimit(Resource::Nofile, limit).unwrap();
    let net = os::net();
    let loopback = config(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0));
    let held: Vec<_> = (0..count).map(|_| net.udp(&loopback).unwrap()).collect();
    let any = config(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
    for i in 0..1_000 {
        let bound = net.udp(&any);
        assert!(bound.is_ok(), "bind {i}: {:?}", bound.err());
    }
    drop(held);
}
