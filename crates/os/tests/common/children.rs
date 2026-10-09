//! What the test binaries of `os` that read the descriptors of a child share: the
//! sockets they open, and the sockets that a new child holds.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::process::Command;

use env::net::tcp::{Listen, Options};
use env::net::udp;

const LOCAL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

pub(crate) const BIND: udp::Config = udp::Config {
    local: LOCAL,
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
};

pub(crate) const OPTIONS: Options = Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
    delayed: false,
};

pub(crate) const LISTEN: Listen = Listen {
    local: LOCAL,
    backlog: 1,
    options: OPTIONS,
};

/// Each socket that a new child holds after its exec, by its path in `/dev/fd`.
pub(crate) fn held() -> Vec<String> {
    let list = r#"for f in /dev/fd/*; do if [ -S "$f" ]; then echo "$f"; fi; done"#;
    let listed = (Command::new("sh").args(["-c", list]).output()).expect("run sh");
    assert!(listed.status.success(), "sh lists the descriptors");
    let listed = String::from_utf8(listed.stdout).expect("sh writes UTF-8");
    listed.lines().map(String::from).collect()
}
