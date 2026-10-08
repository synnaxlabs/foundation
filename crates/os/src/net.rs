//! TCP streams and listeners on the real network. Each one is a non-blocking socket
//! that registers with the I/O driver of the Tokio runtime of the thread of its first
//! poll.

use std::ffi::c_int;
use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use env::net::{Connect, Error, Resolve, tcp, udp};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};
use rustix::net::{AddressFamily, SocketType, ipproto, sockopt};
use tokio::net::TcpStream;

use self::listener::Listener;
use self::stream::Stream;

mod listener;
mod socket;
mod stream;

/// The network of this machine.
pub(crate) struct Driver;

impl env::net::Driver for Driver {
    fn udp(&self, _: &udp::Config) -> Result<Box<dyn udp::Driver>, Error> {
        panic!("os::net has no UDP driver yet")
    }

    fn connect<'a>(&'a self, config: &'a tcp::Config) -> Connect<'a> {
        Box::pin(connect(config))
    }

    fn listen(
        &self,
        config: &tcp::Listen,
    ) -> Result<Box<dyn env::net::listener::Driver>, Error> {
        Ok(Box::new(Listener::listen(config)?))
    }

    fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Resolve<'a> {
        panic!("os::net has no resolver yet")
    }
}

/// Connects to `config.remote` through the I/O driver of the runtime that polls the
/// future, and gives the stream back on no I/O driver.
async fn connect(config: &tcp::Config) -> Result<Box<dyn tcp::Driver>, Error> {
    let remote = config.remote;
    let stream =
        connecting(remote, &config.options).map_err(|e| stream_error(e, remote))?;
    let stream =
        TcpStream::from_std(stream).map_err(|e| stream_error(errno(&e), remote))?;
    stream
        .writable()
        .await
        .map_err(|e| stream_error(errno(&e), remote))?;
    match sockopt::socket_error(&stream) {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => return Err(stream_error(e, remote)),
    }
    let stream = stream
        .into_std()
        .map_err(|e| stream_error(errno(&e), remote))?;
    let local = stream.local_addr().map_err(|e| io_error(errno(&e)))?;
    Ok(Box::new(Stream::new(stream, local, remote)))
}

/// A socket with `options` whose connect to `remote` has started.
fn connecting(
    remote: SocketAddr,
    options: &tcp::Options,
) -> Result<std::net::TcpStream, Errno> {
    let fd = socket(remote)?;
    apply(fd.as_fd(), options)?;
    match rustix::net::connect(&fd, &remote) {
        Ok(()) | Err(Errno::INPROGRESS) => Ok(std::net::TcpStream::from(fd)),
        Err(e) => Err(e),
    }
}

/// A non-blocking TCP socket of the family of `address`, closed on exec. On macOS, a
/// write to a reset socket gives `EPIPE` with no `SIGPIPE`, as `send` with
/// `MSG_NOSIGNAL` does on Linux.
fn socket(address: SocketAddr) -> Result<OwnedFd, Errno> {
    let family = match address {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    let fd = rustix::net::socket(family, SocketType::STREAM, Some(ipproto::TCP))?;
    rustix::io::fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&fd, OFlags::NONBLOCK)?;
    #[cfg(target_os = "macos")]
    sockopt::set_socket_nosigpipe(&fd, true)?;
    Ok(fd)
}

/// Sets `options` on a stream socket. `SO_LINGER` 0 makes a close before `poll_close`
/// reset the peer.
fn apply(fd: BorrowedFd<'_>, options: &tcp::Options) -> Result<(), Errno> {
    sockopt::set_socket_send_buffer_size(fd, options.send_buffer_bytes)?;
    sockopt::set_socket_recv_buffer_size(fd, options.recv_buffer_bytes)?;
    sockopt::set_tcp_nodelay(fd, !options.delayed)?;
    sockopt::set_socket_linger(fd, Some(Duration::ZERO))?;
    set_unsent_max(fd, options.unsent_bytes_max)
}

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

/// Sets `TCP_NOTSENT_LOWAT`, which `rustix` has no call for.
fn set_unsent_max(fd: BorrowedFd<'_>, bytes: usize) -> Result<(), Errno> {
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
        Err(errno(&io::Error::last_os_error()))
    }
}

/// The OS code of `error`, or `EIO` when it has none.
fn errno(error: &io::Error) -> Errno {
    Errno::from_io_error(error).unwrap_or(Errno::IO)
}

/// The error of a socket call that failed with `code`, with no remote to name.
fn io_error(code: Errno) -> Error {
    Error::Io {
        code: code.raw_os_error(),
    }
}

/// The error of a stream to `remote` that failed with `code`.
fn stream_error(code: Errno, remote: SocketAddr) -> Error {
    match code {
        Errno::CONNREFUSED => Error::Refused { remote },
        Errno::NETUNREACH | Errno::HOSTUNREACH => Error::Unreachable { remote },
        Errno::CONNRESET | Errno::PIPE => Error::Reset { remote },
        Errno::TIMEDOUT => Error::TimedOut { remote },
        _ => Error::Io {
            code: code.raw_os_error(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
    }

    fn options(delayed: bool) -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
            unsent_bytes_max: 1 << 14,
            delayed,
        }
    }

    /// Reads `TCP_NOTSENT_LOWAT` back.
    fn unsent_max(fd: BorrowedFd<'_>) -> c_int {
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
        assert_eq!(rc, 0, "getsockopt: {}", io::Error::last_os_error());
        value
    }

    /// Linux keeps twice the buffer size it is given, for its own bookkeeping.
    fn kept(bytes: usize) -> usize {
        if cfg!(target_os = "linux") {
            bytes * 2
        } else {
            bytes
        }
    }

    mod apply {
        use super::*;

        fn check(delayed: bool) {
            let fd = socket(loopback()).unwrap();
            apply(fd.as_fd(), &options(delayed)).unwrap();
            assert_eq!(sockopt::socket_send_buffer_size(&fd), Ok(kept(1 << 16)));
            assert_eq!(sockopt::socket_recv_buffer_size(&fd), Ok(kept(1 << 15)));
            assert_eq!(sockopt::tcp_nodelay(&fd), Ok(!delayed));
            assert_eq!(sockopt::socket_linger(&fd), Ok(Some(Duration::ZERO)));
            assert_eq!(unsent_max(fd.as_fd()), 1 << 14);
        }

        #[test]
        fn sets_each_option_with_nagle_off() {
            check(false);
        }

        #[test]
        fn sets_each_option_with_nagle_on() {
            check(true);
        }

        #[test]
        #[cfg(target_pointer_width = "64")]
        fn refuses_an_unsent_bound_past_a_c_int() {
            let fd = socket(loopback()).unwrap();
            let bound = usize::try_from(c_int::MAX).unwrap() + 1;
            assert_eq!(set_unsent_max(fd.as_fd(), bound), Err(Errno::INVAL));
            assert_eq!(set_unsent_max(fd.as_fd(), bound - 1), Ok(()));
        }
    }

    mod socket {
        use super::*;

        #[test]
        fn is_non_blocking_and_closed_on_exec() {
            let fd = super::socket(loopback()).unwrap();
            assert!(
                rustix::fs::fcntl_getfl(&fd)
                    .unwrap()
                    .contains(OFlags::NONBLOCK)
            );
            assert!(
                rustix::io::fcntl_getfd(&fd)
                    .unwrap()
                    .contains(FdFlags::CLOEXEC)
            );
            #[cfg(target_os = "macos")]
            assert_eq!(sockopt::socket_nosigpipe(&fd), Ok(true));
        }

        #[test]
        fn follows_the_family_of_the_address() {
            let v6 = SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 0);
            let fd = super::socket(v6).unwrap();
            assert_eq!(
                rustix::net::sockopt::socket_domain(&fd),
                Ok(AddressFamily::INET6)
            );
            let fd = super::socket(loopback()).unwrap();
            assert_eq!(
                rustix::net::sockopt::socket_domain(&fd),
                Ok(AddressFamily::INET)
            );
        }
    }

    mod stream_error {
        use super::*;

        #[test]
        fn names_the_remote_in_each_reach_failure() {
            let remote: SocketAddr = "10.0.0.2:4433".parse().unwrap();
            let cases = [
                (Errno::CONNREFUSED, Error::Refused { remote }),
                (Errno::NETUNREACH, Error::Unreachable { remote }),
                (Errno::HOSTUNREACH, Error::Unreachable { remote }),
                (Errno::CONNRESET, Error::Reset { remote }),
                (Errno::PIPE, Error::Reset { remote }),
                (Errno::TIMEDOUT, Error::TimedOut { remote }),
                (
                    Errno::ADDRNOTAVAIL,
                    Error::Io {
                        code: Errno::ADDRNOTAVAIL.raw_os_error(),
                    },
                ),
            ];
            for (code, expected) in cases {
                assert_eq!(stream_error(code, remote), expected, "{code:?}");
            }
        }
    }

    mod errno {
        use super::*;

        #[test]
        fn gives_the_os_code_or_eio() {
            let os = io::Error::from_raw_os_error(Errno::AGAIN.raw_os_error());
            assert_eq!(errno(&os), Errno::AGAIN);
            assert_eq!(errno(&io::Error::other("no code")), Errno::IO);
        }
    }
}
