//! A TCP listener: the kernel's socket, polled through Tokio.

use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::task::{Context, Poll, ready};

use env::net::{Error, listener, tcp};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};
use rustix::net::{AddressFamily, SocketType, ipproto, sockopt};
use tokio::net::{TcpListener, TcpStream};

use super::socket::Socket;
use super::stream::Stream;
use super::{apply, errno, io_error};

/// A listening socket. Each accepted stream gets `options`.
pub(super) struct Listener {
    socket: Socket<std::net::TcpListener, TcpListener>,
    local: SocketAddr,
    options: tcp::Options,
}

impl Listener {
    /// Listens on `config.local`, with `SO_REUSEADDR` so a restart binds a port in
    /// `TIME_WAIT`. Needs no runtime: Tokio's own listen would register the socket
    /// with the runtime of this thread at once.
    pub(super) fn listen(config: &tcp::Listen) -> Result<Self, Error> {
        let local = config.local;
        let in_use = |code| match code {
            Errno::ADDRINUSE => Error::AddressInUse { local },
            code => io_error(code),
        };
        let fd = socket(local).map_err(io_error)?;
        sockopt::set_socket_reuseaddr(&fd, true).map_err(io_error)?;
        // Linux sizes the window of each stream from the buffers of the listener.
        apply(fd.as_fd(), &config.options).map_err(io_error)?;
        rustix::net::bind(&fd, &local).map_err(in_use)?;
        let backlog = i32::try_from(config.backlog).unwrap_or(i32::MAX);
        rustix::net::listen(&fd, backlog).map_err(in_use)?;
        let listener = std::net::TcpListener::from(fd);
        let local = listener.local_addr().map_err(|e| io_error(errno(&e)))?;
        Ok(Self {
            socket: Socket::Idle(listener),
            local,
            options: config.options,
        })
    }
}

/// A non-blocking TCP socket of the family of `address`, closed on exec. On macOS,
/// `SO_NOSIGPIPE` makes a write to a reset socket give `EPIPE` with no `SIGPIPE`. On
/// Linux the `writev` of Tokio sends no `MSG_NOSIGNAL`, and the `SIGPIPE` ignore that
/// std sets at startup does that.
pub(super) fn socket(address: SocketAddr) -> Result<OwnedFd, Errno> {
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

/// `address` as `sim` names it: an IPv4 peer of an IPv6 listener is an IPv4 address.
fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

/// A stream the kernel accepted, with the options of the listener set: not every OS
/// hands the TCP options of the listener to its streams.
fn accepted(
    stream: TcpStream,
    peer: SocketAddr,
    options: &tcp::Options,
) -> Result<Stream, Error> {
    apply(stream.as_fd(), options).map_err(io_error)?;
    let stream = stream.into_std().map_err(|e| io_error(errno(&e)))?;
    let local = stream.local_addr().map_err(|e| io_error(errno(&e)))?;
    Stream::new(stream, canonical(local), canonical(peer)).map_err(io_error)
}

impl listener::Driver for Listener {
    fn local(&self) -> SocketAddr {
        self.local
    }

    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Box<dyn tcp::Driver>, Error>> {
        let listener = self
            .socket
            .live("listener", TcpListener::from_std)
            .map_err(io_error)?;
        let (stream, peer) =
            ready!(listener.poll_accept(cx)).map_err(|e| io_error(errno(&e)))?;
        let stream = accepted(stream, peer, &self.options)?;
        Poll::Ready(Ok(Box::new(stream)))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
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
            let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0);
            let fd = super::socket(v6).unwrap();
            assert_eq!(sockopt::socket_domain(&fd), Ok(AddressFamily::INET6));
            let fd = super::socket(loopback()).unwrap();
            assert_eq!(sockopt::socket_domain(&fd), Ok(AddressFamily::INET));
        }
    }

    mod canonical {
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
    }
}
