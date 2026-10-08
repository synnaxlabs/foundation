//! A UDP socket on which Linux refuses GSO, as on a card that cannot segment.

use std::net::SocketAddr;
use std::os::fd::BorrowedFd;

/// Turns off the UDP checksum of the socket bound to `local`, so that Linux refuses
/// each GSO send on it with `EINVAL`, as a card that cannot segment does.
#[expect(
    unsafe_code,
    reason = "the socket of `os` is reached by its descriptor"
)]
pub(crate) fn refuse(local: SocketAddr) {
    let fds = std::fs::read_dir("/proc/self/fd").expect("procfs is mounted");
    let fd = fds
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .find(|&fd| {
            // SAFETY: the descriptor stays open for this call: the caller holds
            // the socket, and another descriptor that closes gives only an error.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            rustix::net::getsockname(fd)
                .ok()
                .and_then(|name| SocketAddr::try_from(name).ok())
                == Some(local)
        })
        .expect("the socket is open");
    let one: libc::c_int = 1;
    // SAFETY: `one` outlives the call, and its size is the length given.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NO_CHECK,
            (&raw const one).cast(),
            libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("an int fits"),
        )
    };
    assert_eq!(rc, 0, "SO_NO_CHECK is set");
}
