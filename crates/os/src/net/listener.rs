//! A TCP listener: the kernel's socket, polled through Tokio.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::task::{Context, Poll, ready};

use env::net::{Error, listener, tcp};
use rustix::io::Errno;
use rustix::net::{SocketType, ipproto, sockopt};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use super::socket::Socket;
use super::stream::Stream;
use super::{apply, bind, canonical, from_io, in_use, io_error};

/// A listening socket.
pub(super) struct Listener {
    socket: Socket<std::net::TcpListener, AsyncFd<std::net::TcpListener>>,
    local: SocketAddr,
    /// Set again on each accepted stream.
    options: tcp::Options,
}

impl Listener {
    /// Listens on `config.local`, with `SO_REUSEADDR` so a restart binds a port in
    /// `TIME_WAIT`. Needs no runtime: Tokio's own listen would register the socket
    /// with the runtime of this thread at once.
    pub(super) fn listen(config: &tcp::Listen) -> Result<Self, Error> {
        let local = config.local;
        let fd = socket(local).map_err(io_error)?;
        sockopt::set_socket_reuseaddr(&fd, true).map_err(io_error)?;
        // The window scale of each accepted stream comes from the receive buffer of
        // the listener. macOS does not copy it to the stream, so `accepted` sets each
        // option again.
        apply(fd.as_fd(), &config.options).map_err(io_error)?;
        bind(fd.as_fd(), local)?;
        listen(fd.as_fd(), local, config.backlog)?;
        let listener = std::net::TcpListener::from(fd);
        let local = listener.local_addr().map_err(|e| from_io(&e))?;
        Ok(Self {
            socket: Socket::new(listener),
            local: canonical(local),
            options: config.options,
        })
    }

    /// A stream the kernel accepted from `peer`, with the options of the listener.
    fn accepted(
        &self,
        stream: std::net::TcpStream,
        peer: SocketAddr,
    ) -> Result<Stream, Error> {
        let local = stream.local_addr().map_err(|e| from_io(&e))?;
        Stream::new(
            stream,
            canonical(local),
            canonical(peer),
            &self.options,
            None,
        )
        .map_err(io_error)
    }
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
    let fd = super::socket(address, SocketType::STREAM, ipproto::TCP)?;
    #[cfg(target_os = "macos")]
    sockopt::set_socket_nosigpipe(&fd, true)?;
    Ok(fd)
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(fd) = self.socket.fd() {
            stop(fd);
        }
    }
}

/// Stops the listen of `fd`. A child that another thread spawns holds a copy of the
/// socket until its exec. On Linux a shutdown stops the listen of each copy, so a
/// connect is refused at once. macOS gives `ENOTCONN` for a shutdown of a listener.
#[cfg_attr(
    not(target_os = "linux"),
    expect(unused_variables, reason = "macOS has no call that stops the listen")
)]
fn stop(fd: BorrowedFd<'_>) {
    #[cfg(target_os = "linux")]
    match rustix::net::shutdown(fd, rustix::net::Shutdown::Read) {
        // An operator aborted the socket (`ss -K`), which stopped its listen.
        Ok(()) | Err(Errno::NOTCONN) => {}
        Err(e) => panic!("invariant: the listener {fd:?} stops its listen once: {e:?}"),
    }
}

/// Registers `listener` with the I/O driver of this thread. A failed registration
/// stops the listen before the socket closes.
fn register(
    listener: std::net::TcpListener,
) -> io::Result<AsyncFd<std::net::TcpListener>> {
    AsyncFd::try_with_interest(listener, Interest::READABLE).map_err(|failed| {
        let (listener, error) = failed.into_parts();
        stop(listener.as_fd());
        error
    })
}

/// Accepts one stream, non-blocking and closed on exec, or registers `cx` for the next.
fn accept(
    listener: &AsyncFd<std::net::TcpListener>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<(std::net::TcpStream, SocketAddr)>> {
    loop {
        let mut ready = ready!(listener.poll_read_ready(cx))?;
        if let Ok(accepted) = ready.try_io(|listener| listener.get_ref().accept()) {
            return Poll::Ready(accepted.and_then(|(stream, peer)| {
                stream.set_nonblocking(true)?;
                Ok((stream, peer))
            }));
        }
    }
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
            .live("TCP listener", register)
            .map_err(io_error)?;
        let (stream, peer) = ready!(accept(listener, cx)).map_err(|e| from_io(&e))?;
        let stream = self.accepted(stream, peer)?;
        Poll::Ready(Ok(Box::new(stream)))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::num::NonZeroUsize;

    use rustix::fs::OFlags;
    use rustix::io::FdFlags;

    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
    }

    fn options() -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
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
        let _client =
            std::net::TcpStream::connect(listener::Driver::local(&listener)).unwrap();
        // The test accepts on the descriptor, so a copy of it reads the options that
        // `accepted` sets.
        let fd = listener.socket.fd().unwrap();
        // macOS can queue the connection after `connect` returns.
        rustix::fs::fcntl_setfl(fd, OFlags::empty()).unwrap();
        let fd = rustix::net::accept(fd).unwrap();
        let peer = rustix::net::getpeername(&fd).unwrap().unwrap();
        // A copy of the descriptor sees the options of the socket.
        let accepted = rustix::io::dup(&fd).unwrap();
        let peer = peer.try_into().unwrap();
        let _stream = listener.accepted(fd.into(), peer).unwrap();
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
