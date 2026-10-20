//! Only Linux: the test refuses calls with a seccomp filter.
//!
//! A `[::]` bind on port 0 under a policy that refuses each bind to a fixed port, as
//! systemd `SocketBindDeny=any` does with its cgroup bind hook: a bind on port 0 goes
//! through, and a bind on any port above 0 gets `EPERM`.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

#[path = "common/seccomp.rs"]
mod seccomp;

use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use env::net::udp;

/// Makes each bind on a port above 0 fail with `EPERM`, and counts those binds.
fn deny_fixed_ports() -> Arc<AtomicUsize> {
    let denied = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&denied);
    seccomp::answer_calls(&[libc::SYS_bind], move |data, _| {
        let address = data.args[1] as *const libc::sockaddr_in;
        // SAFETY: the filter holds a call of this process, whose address is valid
        // while the call waits. `sin_port` has the same offset in `sockaddr_in6`.
        let port = unsafe { (*address).sin_port };
        let refused = port != 0;
        if refused {
            count.fetch_add(1, Ordering::Relaxed);
        }
        refused.then_some(libc::EPERM)
    });
    denied
}

/// The kernel picks after 8 probe ports fail to bind (#3422).
#[test]
fn port_0_on_any_v6_binds_after_8_denied_ports() {
    let net = os::net();
    let denied = deny_fixed_ports();
    let local = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
    let config = udp::Config {
        local,
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    assert_eq!(net.udp(&config).map(drop), Ok(()));
    assert_eq!(denied.load(Ordering::Relaxed), 8);
}
