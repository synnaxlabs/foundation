//! A TCP listener: the kernel's socket, polled through Tokio.

use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::task::{Context, Poll, ready};

use env::net::{Error, listener, tcp};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};
use rustix::net::{AddressFamily, SocketType, ipproto, sockopt};
use tokio::net::{TcpListener, TcpStream};

use super::socket::Socket;
use super::stream::Stream;
use super::{apply, canonical, errno, io_error};

/// A listening socket.
pub(super) struct Listener {
    socket: Socket<std::net::TcpListener, TcpListener>,
    local: SocketAddr,
}

impl Listener {
    /// Listens on `config.local`, with `SO_REUSEADDR` so a restart binds a port in
    /// `TIME_WAIT`. Needs no runtime: Tokio's own listen would register the socket
    /// with the runtime of this thread at once.
    pub(super) fn listen(config: &tcp::Listen) -> Result<Self, Error> {
        let local = config.local;
        let fd = socket(local).map_err(io_error)?;
        sockopt::set_socket_reuseaddr(&fd, true).map_err(io_error)?;
        // Each accepted stream inherits the options, and Linux sizes the window of
        // each stream from the buffers of the listener.
        apply(fd.as_fd(), &config.options).map_err(io_error)?;
        bind(fd.as_fd(), local)?;
        listen(fd.as_fd(), local, config.backlog)?;
        let listener = std::net::TcpListener::from(fd);
        let local = listener.local_addr().map_err(|e| io_error(errno(&e)))?;
        Ok(Self {
            socket: Socket::Idle(listener),
            local: canonical(local),
        })
    }
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

/// Makes `fd`, bound to `local`, listen. Linux lets two `SO_REUSEADDR` sockets bind
/// one address while neither listens, and refuses the second `listen`.
fn listen(fd: BorrowedFd<'_>, local: SocketAddr, backlog: u32) -> Result<(), Error> {
    let backlog = i32::try_from(backlog).unwrap_or(i32::MAX);
    rustix::net::listen(fd, backlog).map_err(in_use(local))
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

/// A stream the kernel accepted, with the options of the listener inherited.
fn accepted(stream: TcpStream, peer: SocketAddr) -> Result<Stream, Error> {
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
        let stream = accepted(stream, peer)?;
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

    fn options() -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
            unsent_bytes_max: 1 << 14,
            delayed: true,
        }
    }

    /// A socket with `SO_REUSEADDR`, bound to `local`.
    fn bound(local: SocketAddr) -> (OwnedFd, SocketAddr) {
        let fd = socket(local).unwrap();
        sockopt::set_socket_reuseaddr(&fd, true).unwrap();
        bind(fd.as_fd(), local).unwrap();
        let local = rustix::net::getsockname(&fd).unwrap();
        (fd, local.try_into().unwrap())
    }

    #[test]
    fn a_bind_to_a_listening_address_is_in_use() {
        let (first, local) = bound(loopback());
        listen(first.as_fd(), local, 1).unwrap();
        let second = socket(local).unwrap();
        sockopt::set_socket_reuseaddr(&second, true).unwrap();
        assert_eq!(
            bind(second.as_fd(), local),
            Err(Error::AddressInUse { local })
        );
    }

    /// macOS refuses the second bind instead.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_second_listen_on_a_bound_address_is_in_use() {
        let (first, local) = bound(loopback());
        let (second, _) = bound(local);
        assert_eq!(listen(first.as_fd(), local, 1), Ok(()));
        assert_eq!(
            listen(second.as_fd(), local, 1),
            Err(Error::AddressInUse { local })
        );
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    fn an_accepted_socket_has_the_options_of_the_listener() {
        let config = tcp::Listen {
            local: loopback(),
            backlog: 1,
            options: options(),
        };
        let listener = Listener::listen(&config).unwrap();
        let _client = std::net::TcpStream::connect(listener.local).unwrap();
        let fd = listener.socket.fd().unwrap();
        // macOS can queue the connection after `connect` returns.
        rustix::fs::fcntl_setfl(fd, OFlags::empty()).unwrap();
        let accepted = rustix::net::accept(fd).unwrap();
        let kept = super::super::tests::kept;
        let sent = sockopt::socket_send_buffer_size(&accepted).unwrap();
        if cfg!(target_os = "macos") {
            // macOS rounds it up to whole segments, of at most 16 KiB on loopback.
            assert!((1 << 16..(1 << 16) + (1 << 14)).contains(&sent), "{sent}");
        } else {
            assert_eq!(sent, kept(1 << 16));
        }
        assert_eq!(
            sockopt::socket_recv_buffer_size(&accepted),
            Ok(kept(1 << 15))
        );
        assert_eq!(sockopt::tcp_nodelay(&accepted), Ok(false));
        assert_eq!(super::super::lowat::get(accepted.as_fd()), Ok(1 << 14));
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

        /// The kernel binds a socket only to an address of its own family.
        fn bound(address: SocketAddr) -> SocketAddr {
            let fd = super::socket(address).unwrap();
            rustix::net::bind(&fd, &address).unwrap();
            SocketAddr::try_from(rustix::net::getsockname(&fd).unwrap()).unwrap()
        }

        #[test]
        fn follows_the_family_of_the_address() {
            let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0);
            assert!(bound(v6).is_ipv6());
            assert!(bound(loopback()).is_ipv4());
        }
    }
}
