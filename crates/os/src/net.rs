//! UDP sockets, TCP streams, listeners, and name lookups on the real network. Each
//! socket is non-blocking. `os::net()` states when each registers.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use env::net::{Connect, Error, Resolve, tcp};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};
use rustix::net::{AddressFamily, Protocol, SocketType, sockopt};
use tokio::net::TcpStream;

use self::listener::Listener;
use self::stream::Stream;
use self::udp::Udp;

mod listener;
#[expect(
    unsafe_code,
    reason = "`TCP_NOTSENT_LOWAT` is a `setsockopt` rustix lacks"
)]
mod lowat;
#[expect(unsafe_code, reason = "`getaddrinfo` is the C library's name lookup")]
mod resolve;
mod socket;
mod stream;
mod udp;
#[cfg(target_os = "macos")]
mod unsent;

/// The network of this machine.
pub(crate) struct Driver;

impl env::net::Driver for Driver {
    fn udp(
        &self,
        config: &env::net::udp::Config,
    ) -> Result<
        (
            Box<dyn env::net::udp::Driver>,
            Box<dyn env::net::udp::receiver::Driver>,
        ),
        Error,
    > {
        let (udp, receiver) = Udp::bind(config)?;
        Ok((Box::new(udp), Box::new(receiver)))
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

    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> Resolve<'a> {
        Box::pin(resolve::lookup(host, port))
    }
}

/// Connects to `config.remote` through the I/O driver of the runtime that polls the
/// future. The stream it gives is registered with no driver yet, so any thread may
/// take it. A peer that resets after the handshake gives a stream that reads
/// [`Error::Reset`], as in `sim`.
async fn connect(config: &tcp::Config) -> Result<Box<dyn tcp::Driver>, Error> {
    let remote = canonical(config.remote);
    let failed = |code: Errno| stream_error(code, remote);
    let fd = listener::socket(config.remote).map_err(failed)?;
    apply(fd.as_fd(), &config.options).map_err(failed)?;
    match rustix::net::connect(&fd, &config.remote) {
        Ok(()) | Err(Errno::INPROGRESS) => {}
        Err(code) => return Err(failed(code)),
    }
    let stream = TcpStream::from_std(fd.into()).map_err(|e| failed(errno(&e)))?;
    stream.writable().await.map_err(|e| failed(errno(&e)))?;
    // Before `take_error`, so a reset that removed the peer is still pending.
    let named = stream.peer_addr();
    let reset = match stream.take_error() {
        Ok(None) => None,
        Ok(Some(e)) | Err(e) => match failed(errno(&e)) {
            reset @ Error::Reset { .. } => Some(reset),
            error => return Err(error),
        },
    };
    let stream = stream.into_std().map_err(|e| failed(errno(&e)))?;
    let local = stream.local_addr().map_err(|e| from_io(&e))?;
    let peer = peer(named, remote, reset.is_some())?;
    // With a reset, the kernel gave it to `take_error`, so a read would see an end of
    // stream.
    let stream = Stream::new(stream, canonical(local), peer, &config.options, reset)
        .map_err(failed)?;
    Ok(Box::new(stream))
}

/// The peer that the kernel `named` for a stream connected to `remote`: without a
/// scope or flow label the kernel does not use, and with the address an unspecified
/// `remote` reached. After a `reset` the kernel holds no peer, so the peer is
/// `remote`, which `connect` has already passed through `canonical`.
fn peer(
    named: std::io::Result<SocketAddr>,
    remote: SocketAddr,
    reset: bool,
) -> Result<SocketAddr, Error> {
    match named {
        Ok(peer) => Ok(canonical(peer)),
        Err(_) if reset => Ok(remote),
        Err(e) => Err(from_io(&e)),
    }
}

/// `address` as `sim` names it: an IPv4 address on an IPv6 socket is an IPv4 address.
/// Any other address keeps its scope and flow label.
fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => address,
        },
        SocketAddr::V4(_) => address,
    }
}

/// A non-blocking socket of the family of `address`, closed on exec.
fn socket(
    address: SocketAddr,
    kind: SocketType,
    protocol: Protocol,
) -> Result<OwnedFd, Errno> {
    let family = match address {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    let fd = rustix::net::socket(family, kind, Some(protocol))?;
    rustix::io::fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&fd, OFlags::NONBLOCK)?;
    Ok(fd)
}

/// `EADDRINUSE` on `local` is `AddressInUse`.
fn in_use(local: SocketAddr) -> impl Fn(Errno) -> Error {
    move |code| match code {
        Errno::ADDRINUSE => Error::AddressInUse { local },
        code => io_error(code),
    }
}

/// Binds `fd` to `local`.
fn bind(fd: BorrowedFd<'_>, local: SocketAddr) -> Result<(), Error> {
    rustix::net::bind(fd, &local).map_err(in_use(local))
}

/// Sets `options` on a TCP socket.
fn apply(fd: BorrowedFd<'_>, options: &tcp::Options) -> Result<(), Errno> {
    sockopt::set_socket_send_buffer_size(fd, options.send_buffer_bytes)?;
    sockopt::set_socket_recv_buffer_size(fd, options.recv_buffer_bytes)?;
    sockopt::set_tcp_nodelay(fd, !options.delayed)?;
    lowat::set(fd, options.unsent_bytes_max)
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

/// The error of a socket call that failed with `error`, with no remote to name.
fn from_io(error: &io::Error) -> Error {
    io_error(errno(error))
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

    /// Linux keeps twice the buffer size it is given, for its own bookkeeping.
    pub(super) fn kept(bytes: usize) -> usize {
        if cfg!(target_os = "linux") {
            bytes * 2
        } else {
            bytes
        }
    }

    mod apply {
        use super::*;

        fn check(delayed: bool) {
            let fd = listener::socket(loopback()).unwrap();
            apply(fd.as_fd(), &options(delayed)).unwrap();
            assert_eq!(sockopt::socket_send_buffer_size(&fd), Ok(kept(1 << 16)));
            assert_eq!(sockopt::socket_recv_buffer_size(&fd), Ok(kept(1 << 15)));
            assert_eq!(sockopt::tcp_nodelay(&fd), Ok(!delayed));
            assert_eq!(lowat::get(fd.as_fd()), Ok(1 << 14));
            assert_eq!(sockopt::socket_linger(&fd), Ok(None), "the stream sets it");
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
        fn gives_the_code_of_a_refused_option() {
            let fd = listener::socket(loopback()).unwrap();
            let mut options = options(false);
            options.unsent_bytes_max = usize::MAX;
            assert_eq!(apply(fd.as_fd(), &options), Err(Errno::INVAL));
        }
    }

    mod canonical {
        use std::net::SocketAddrV6;

        use super::*;

        #[test]
        fn unmaps_an_ipv4_address_and_keeps_the_rest() {
            let mapped: SocketAddr = "[::ffff:127.0.0.1]:8080".parse().unwrap();
            let plain: SocketAddr = "127.0.0.1:8080".parse().unwrap();
            assert_eq!(canonical(mapped), plain);
            assert_eq!(canonical(plain), plain);
            let v6: SocketAddr = "[::1]:8080".parse().unwrap();
            assert_eq!(canonical(v6), v6);
        }

        #[test]
        fn keeps_the_scope_and_flow_label_of_an_ipv6_address() {
            let link_local = SocketAddrV6::new("fe80::1".parse().unwrap(), 8080, 7, 2);
            let address = SocketAddr::V6(link_local);
            assert_eq!(canonical(address), address);
        }
    }

    mod peer {
        use std::time::Duration;

        use super::*;

        #[test]
        #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
        fn a_peer_that_reset_after_the_connect_is_the_remote() {
            let listener = std::net::TcpListener::bind(loopback()).unwrap();
            let remote = listener.local_addr().unwrap();
            let client = std::net::TcpStream::connect(remote).unwrap();
            let (server, _) = listener.accept().unwrap();
            // macOS refuses an option on a socket after a reset.
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            sockopt::set_socket_linger(&server, Some(Duration::ZERO)).unwrap();
            drop(server);
            let read = client.peek(&mut [0; 1]).map_err(|e| e.raw_os_error());
            assert_eq!(read, Err(Some(Errno::CONNRESET.raw_os_error())));
            let none = if cfg!(target_os = "macos") {
                Errno::INVAL
            } else {
                Errno::NOTCONN
            };
            assert_eq!(client.peer_addr().map_err(|e| errno(&e)), Err(none));
            assert_eq!(peer(client.peer_addr(), remote, true), Ok(remote));
        }

        #[test]
        fn no_peer_without_a_reset_gives_its_code() {
            let remote = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 4433);
            let named = Err(Errno::INVAL.into());
            let code = Errno::INVAL.raw_os_error();
            assert_eq!(peer(named, remote, false), Err(Error::Io { code }));
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
