//! A TCP stream: the kernel's socket, polled through Tokio.

use std::io::IoSlice;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use env::net::{Error, tcp};
use rustix::io::Errno;
use rustix::net::{Shutdown, sockopt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use super::socket::Socket;
use super::{errno, io_error, stream_error};

/// A connected stream. `SO_LINGER` 0 is set until `poll_close`, so a drop before it
/// resets the peer.
pub(super) struct Stream {
    socket: Socket<std::net::TcpStream, TcpStream>,
    local: SocketAddr,
    peer: SocketAddr,
    /// `poll_close` ran: the FIN is queued.
    closed: bool,
}

impl Stream {
    /// A stream over `stream`, connected from `local` to `peer`, before its first poll.
    pub(super) fn new(
        stream: std::net::TcpStream,
        local: SocketAddr,
        peer: SocketAddr,
    ) -> Self {
        Self {
            socket: Socket::Idle(stream),
            local,
            peer,
            closed: false,
        }
    }

    fn live(&mut self) -> Result<Pin<&mut TcpStream>, Error> {
        let stream = self
            .socket
            .live("stream", TcpStream::from_std)
            .map_err(io_error)?;
        Ok(Pin::new(stream))
    }
}

impl tcp::Driver for Stream {
    fn local(&self) -> SocketAddr {
        self.local
    }

    fn peer(&self) -> SocketAddr {
        self.peer
    }

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        let peer = self.peer;
        let mut read = ReadBuf::new(buffer);
        ready!(self.live()?.poll_read(cx, &mut read))
            .map_err(|e| stream_error(errno(&e), peer))?;
        Poll::Ready(Ok(read.filled().len()))
    }

    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        if self.closed {
            return Poll::Ready(Err(io_error(Errno::PIPE)));
        }
        let peer = self.peer;
        let written = ready!(self.live()?.poll_write_vectored(cx, buffers));
        Poll::Ready(written.map_err(|e| stream_error(errno(&e), peer)))
    }

    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if self.closed {
            return Poll::Ready(Ok(()));
        }
        let peer = self.peer;
        let stream = self.live()?;
        match rustix::net::shutdown(&*stream, Shutdown::Write) {
            Ok(()) | Err(Errno::NOTCONN) => {}
            Err(e) => return Poll::Ready(Err(stream_error(e, peer))),
        }
        sockopt::set_socket_linger(&*stream, None)
            .map_err(|e| stream_error(e, peer))?;
        self.closed = true;
        Poll::Ready(Ok(()))
    }
}

impl Drop for Stream {
    /// After `poll_close`, bytes the peer sent and this side did not read make the
    /// close reset, on each OS. Before it, `SO_LINGER` 0 is still set.
    fn drop(&mut self) {
        if !self.closed {
            return;
        }
        let Some(fd) = self.socket.fd() else { return };
        if rustix::io::ioctl_fionread(fd).is_ok_and(|unread| unread > 0) {
            // The close follows at once, so nothing can act on a failure here.
            let _linger = sockopt::set_socket_linger(fd, Some(Duration::ZERO));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::{Ipv4Addr, TcpListener};
    use std::os::fd::{AsFd, OwnedFd};

    use super::*;

    /// A connected pair on the loopback, with the blocking calls of std.
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    fn create_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client =
            std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// Drops a closed stream over `client` and gives a descriptor that still sees the
    /// socket's options.
    fn drop_closed(client: std::net::TcpStream) -> OwnedFd {
        let local = client.local_addr().unwrap();
        let peer = client.peer_addr().unwrap();
        let kept = rustix::io::dup(client.as_fd()).unwrap();
        let mut stream = Stream::new(client, local, peer);
        stream.closed = true;
        drop(stream);
        kept
    }

    #[test]
    fn a_drop_after_close_with_unread_bytes_sets_linger_zero() {
        let (client, mut server) = create_pair();
        server.write_all(b"unread").unwrap();
        assert_eq!(client.peek(&mut [0; 1]).unwrap(), 1, "the bytes arrive");
        let kept = drop_closed(client);
        assert_eq!(sockopt::socket_linger(&kept), Ok(Some(Duration::ZERO)));
    }

    #[test]
    fn a_drop_after_close_with_nothing_unread_keeps_the_linger_off() {
        let (client, _server) = create_pair();
        let kept = drop_closed(client);
        assert_eq!(sockopt::socket_linger(&kept), Ok(None));
    }
}
