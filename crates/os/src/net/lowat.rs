//! `TCP_NOTSENT_LOWAT`, the one socket option `rustix` has no call for.

use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use rustix::io::Errno;

/// libc has no `TCP_NOTSENT_LOWAT` for macOS. The value is from `netinet/tcp.h`.
#[cfg(target_os = "macos")]
const TCP_NOTSENT_LOWAT: c_int = 0x201;
#[cfg(target_os = "linux")]
const TCP_NOTSENT_LOWAT: c_int = libc::TCP_NOTSENT_LOWAT;

/// The size of a `c_int`, as `setsockopt` and `getsockopt` take it.
const C_INT_LEN: libc::socklen_t = 4;
const _: () = assert!(
    size_of::<c_int>() == 4,
    "a c_int is four bytes on each target"
);

/// Sets the bound of unsent bytes of `fd` to `bytes`. Gives `EINVAL` past a `c_int`.
pub(super) fn set(fd: BorrowedFd<'_>, bytes: usize) -> Result<(), Errno> {
    let value = c_int::try_from(bytes).map_err(|_overflow| Errno::INVAL)?;
    // SAFETY: `fd` is open, and the pointer and `len` are those of `value`.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&raw const value).cast(),
            C_INT_LEN,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(super::errno(&io::Error::last_os_error()))
    }
}

/// The bound of unsent bytes of `fd`.
#[cfg(test)]
pub(super) fn get(fd: BorrowedFd<'_>) -> Result<c_int, Errno> {
    let mut value: c_int = 0;
    let mut len = C_INT_LEN;
    // SAFETY: `fd` is open, and the pointers are those of `value` and `len`.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if rc == 0 {
        Ok(value)
    } else {
        Err(super::errno(&io::Error::last_os_error()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::os::fd::AsFd;

    use super::super::listener::socket;
    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
    }

    #[test]
    fn sets_the_bound_the_kernel_reads_back() {
        let fd = socket(loopback()).unwrap();
        assert_eq!(set(fd.as_fd(), 1 << 14), Ok(()));
        assert_eq!(get(fd.as_fd()), Ok(1 << 14));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn refuses_a_bound_past_a_c_int() {
        let fd = socket(loopback()).unwrap();
        let bound = usize::try_from(c_int::MAX).unwrap() + 1;
        assert_eq!(set(fd.as_fd(), bound), Err(Errno::INVAL));
        assert_eq!(set(fd.as_fd(), bound - 1), Ok(()));
    }

    /// A UDP socket has no TCP options.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_socket_with_no_tcp_gives_the_kernel_code() {
        use rustix::net::{AddressFamily, SocketType, ipproto};
        let fd = rustix::net::socket(
            AddressFamily::INET,
            SocketType::DGRAM,
            Some(ipproto::UDP),
        )
        .unwrap();
        assert_eq!(set(fd.as_fd(), 1), Err(Errno::NOPROTOOPT));
        assert_eq!(get(fd.as_fd()), Err(Errno::OPNOTSUPP));
    }
}
