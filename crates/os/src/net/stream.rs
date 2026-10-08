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

/// A connected stream. `SO_LINGER` 0 is set from `new` until `poll_close`, so a drop
/// before it resets the peer.
pub(super) struct Stream {
    socket: Socket<std::net::TcpStream, TcpStream>,
    local: SocketAddr,
    peer: SocketAddr,
    /// `poll_close` ran: the FIN is queued.
    closed: bool,
    /// The error that ended the stream. The kernel reports a reset once and then an
    /// end of stream, so each later poll gives this instead.
    failed: Option<Error>,
}

impl Stream {
    /// A stream over `stream`, connected from `local` to `peer`, before its first poll.
    /// Gives the code of a failed `SO_LINGER`.
    pub(super) fn new(
        stream: std::net::TcpStream,
        local: SocketAddr,
        peer: SocketAddr,
    ) -> Result<Self, Errno> {
        sockopt::set_socket_linger(&stream, Some(Duration::ZERO))?;
        Ok(Self {
            socket: Socket::Idle(stream),
            local,
            peer,
            closed: false,
            failed: None,
        })
    }

    /// Records `error` as the end of the stream, and gives it.
    fn fail(&mut self, error: Error) -> Error {
        self.failed = Some(error.clone());
        error
    }

    fn live(&mut self) -> Result<Pin<&mut TcpStream>, Error> {
        let stream = self
            .socket
            .live("stream", TcpStream::from_std)
            .map_err(io_error)?;
        Ok(Pin::new(stream))
    }

    /// The error the kernel holds for the stream, after an event no poll reported.
    fn pending(stream: &TcpStream) -> Option<Errno> {
        match sockopt::socket_error(stream) {
            Ok(Ok(())) => None,
            Ok(Err(code)) | Err(code) => Some(code),
        }
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
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        let peer = self.peer;
        let mut read = ReadBuf::new(buffer);
        match ready!(self.live()?.poll_read(cx, &mut read)) {
            Ok(()) => Poll::Ready(Ok(read.filled().len())),
            Err(e) => Poll::Ready(Err(self.fail(stream_error(errno(&e), peer)))),
        }
    }

    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        let peer = self.peer;
        let (closed, stream) = (self.closed, self.live()?);
        if closed {
            // A reset after the close is the stream's end. Without one, the write
            // is a misuse, as `sim` reports it.
            return Poll::Ready(Err(match Self::pending(&stream) {
                Some(code) => self.fail(stream_error(code, peer)),
                None => io_error(Errno::PIPE),
            }));
        }
        match ready!(stream.poll_write_vectored(cx, buffers)) {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(e) => Poll::Ready(Err(self.fail(stream_error(errno(&e), peer)))),
        }
    }

    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        let peer = self.peer;
        let (closed, stream) = (self.closed, self.live()?);
        if closed {
            return Poll::Ready(Ok(()));
        }
        // The linger goes first: macOS refuses an option on a socket shut both ways.
        let shut = match sockopt::set_socket_linger(&*stream, None) {
            Ok(()) => match rustix::net::shutdown(&*stream, Shutdown::Write) {
                // The connection ended with no poll that reported why: a reset,
                // unless the kernel holds another code.
                Err(Errno::NOTCONN) => {
                    Err(Self::pending(&stream).unwrap_or(Errno::CONNRESET))
                }
                outcome => outcome,
            },
            Err(e) => Err(e),
        };
        match shut {
            Ok(()) => {
                self.closed = true;
                Poll::Ready(Ok(()))
            }
            Err(code) => Poll::Ready(Err(self.fail(stream_error(code, peer)))),
        }
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
    use std::future::poll_fn;
    use std::io::Write;
    use std::net::{Ipv4Addr, TcpListener};
    use std::os::fd::{AsFd, OwnedFd};

    use tcp::Driver as _;

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

    fn stream(socket: std::net::TcpStream) -> Stream {
        socket.set_nonblocking(true).unwrap();
        let local = socket.local_addr().unwrap();
        let peer = socket.peer_addr().unwrap();
        Stream::new(socket, local, peer).unwrap()
    }

    /// Closes a stream over `client` and drops it, and gives a descriptor that still
    /// sees the socket's options.
    fn drop_closed(client: std::net::TcpStream) -> OwnedFd {
        let kept = rustix::io::dup(client.as_fd()).unwrap();
        let mut stream = stream(client);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(poll_fn(|cx| stream.poll_close(cx)).await, Ok(()));
            drop(stream);
        });
        kept
    }

    #[test]
    fn new_sets_linger_zero() {
        let (client, _server) = create_pair();
        let stream = stream(client);
        let fd = stream.socket.fd().unwrap();
        assert_eq!(sockopt::socket_linger(fd), Ok(Some(Duration::ZERO)));
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
