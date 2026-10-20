//! Only Linux: the test refuses calls with a seccomp filter.
//!
//! A `[::]` bind on port 0 on a host with no free port. The filter acts on each
//! thread that the test thread starts, so it runs in a test binary of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

#[path = "common/seccomp.rs"]
mod seccomp;

use std::net::{Ipv6Addr, SocketAddr};

use env::net::{Error, udp};

/// Makes each bind on port 0 fail with `EADDRINUSE`, as a host with no free port does.
fn no_free_port() {
    seccomp::answer_calls(&[libc::SYS_bind], |data, _| {
        let address = data.args[1] as *const libc::sockaddr_in;
        // SAFETY: the filter holds a call of this process, whose address is valid
        // while the call waits. `sin_port` has the same offset in `sockaddr_in6`.
        let port = unsafe { (*address).sin_port };
        (port == 0).then_some(libc::EADDRINUSE)
    });
}

/// The error names the address of the bind, not that of the probe (#3422).
#[test]
fn a_bind_on_any_v6_port_0_with_no_free_port_names_its_address() {
    let net = os::net();
    no_free_port();
    let local = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
    let config = udp::Config {
        local,
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    assert_eq!(
        net.udp(&config).map(drop),
        Err(Error::AddressInUse { local })
    );
}
