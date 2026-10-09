//! A copy of a socket of the process, as a child holds it from its fork to its exec.
//! It takes a copy of each descriptor of the process for a moment, so each test that
//! calls it runs in a test binary of its own, with that one test only.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;

#[cfg(target_os = "linux")]
use rustix::io::Errno;

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
            let copy = match pidfd_getfd(&pidfd, fd, PidfdGetfdFlags::empty()) {
                Ok(copy) => copy,
                // A copy that this loop closed, which a later read of the directory
                // lists.
                Err(Errno::BADF) => continue,
                Err(e) => panic!("a copy of descriptor {fd}: {e:?}"),
            };
            let name = match rustix::net::getsockname(&copy) {
                Ok(name) => name,
                Err(Errno::NOTSOCK) => continue,
                Err(e) => panic!("the name of descriptor {fd}: {e:?}"),
            };
            if SocketAddr::try_from(name).ok() == Some(local) {
                return copy;
            }
        }
    }
    panic!("no descriptor is bound to {local}");
}
