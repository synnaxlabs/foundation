//! A copy of a socket of the process, as a child holds it from its fork to its exec. It
//! takes a copy of each descriptor of the process up to the socket, one at a time, so
//! each test that calls it runs in a test binary of its own, with that one test only.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;

/// A copy of the socket bound to `local`.
///
/// # Panics
///
/// When no descriptor is bound to `local`, and on each OS but Linux.
pub(crate) fn copy_of(local: SocketAddr) -> OwnedFd {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{PidfdFlags, PidfdGetfdFlags, getpid};
        use rustix::process::{pidfd_getfd, pidfd_open};

        let pidfd = pidfd_open(getpid(), PidfdFlags::empty()).unwrap();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let name = entry.unwrap().file_name();
            let fd = name
                .to_string_lossy()
                .parse()
                .expect("a descriptor is a number");
            let copy = pidfd_getfd(&pidfd, fd, PidfdGetfdFlags::empty())
                .unwrap_or_else(|e| panic!("a copy of descriptor {fd}: {e:?}"));
            // Only the socket must give an IP address. An error from either call
            // means another descriptor: a file, an `O_PATH` descriptor, or a socket
            // of another family.
            let name = rustix::net::getsockname(&copy).ok();
            if name.and_then(|n| SocketAddr::try_from(n).ok()) == Some(local) {
                return copy;
            }
        }
    }
    panic!("no descriptor is bound to {local}");
}
