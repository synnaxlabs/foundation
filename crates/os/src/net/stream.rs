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
    /// end of stream, so each later write, close, and read of no bytes gives this.
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

    /// Takes `socket` alone, so a poll reads `failed` while it holds the stream.
    fn live(
        socket: &mut Socket<std::net::TcpStream, TcpStream>,
    ) -> Result<Pin<&mut TcpStream>, Error> {
        let stream = socket
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
        let peer = self.peer;
        let mut read = ReadBuf::new(buffer);
        // The kernel keeps the bytes that came before a reset, so they come first.
        let outcome =
            match ready!(Self::live(&mut self.socket)?.poll_read(cx, &mut read)) {
                Ok(()) => match (read.filled().len(), &self.failed) {
                    (0, Some(failed)) => Err(failed.clone()),
                    (read, _) => Ok(read),
                },
                Err(e) => Err(self.fail(stream_error(errno(&e), peer))),
            };
        Poll::Ready(outcome)
    }

    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        let peer = self.peer;
        let stream = Self::live(&mut self.socket)?;
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        if self.closed {
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
        let peer = self.peer;
        let stream = Self::live(&mut self.socket)?;
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        if self.closed {
            return Poll::Ready(match Self::pending(&stream) {
                Some(code) => Err(self.fail(stream_error(code, peer))),
                None => Ok(()),
            });
        }
        // The linger goes first: macOS refuses an option on a socket shut both ways.
        let shut = match sockopt::set_socket_linger(&*stream, None) {
            Ok(()) => match rustix::net::shutdown(&*stream, Shutdown::Write) {
                // The connection ended with no poll that reported why.
                Err(Errno::NOTCONN) => {
                    Err(Self::pending(&stream).unwrap_or(Errno::NOTCONN))
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
    use std::task::Waker;

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
        on_runtime(|| async {
            assert_eq!(poll_fn(|cx| stream.poll_close(cx)).await, Ok(()));
            drop(stream);
        });
        kept
    }

    /// Runs `body` on a runtime with an I/O driver and a timer.
    fn on_runtime<T, F: Future<Output = T>>(body: impl FnOnce() -> F) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        runtime.block_on(body())
    }

    /// Linux ends a stream whose peer reads nothing with `ETIMEDOUT` once the window
    /// probes run past `TCP_USER_TIMEOUT`. The first error is the stream's answer for
    /// each later poll, also when it is no reset.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_write_and_a_close_after_a_timeout_the_read_found_give_the_timeout() {
        let (client, server) = create_pair();
        sockopt::set_socket_send_buffer_size(&client, 1 << 16).unwrap();
        sockopt::set_socket_recv_buffer_size(&server, 1 << 12).unwrap();
        sockopt::set_tcp_user_timeout(&client, 1).unwrap();
        let mut stream = stream(client);
        let timed_out = Error::TimedOut {
            remote: stream.peer,
        };
        let bound = Duration::from_secs(10);
        on_runtime(|| {
            tokio::time::timeout(bound, async {
                let block = vec![0; 1 << 16];
                let bytes = [IoSlice::new(&block)];
                // The first poll registers the socket and is pending.
                let mut filled = poll_fn(|cx| stream.poll_write(cx, &bytes)).await;
                let mut cx = Context::from_waker(Waker::noop());
                while let Poll::Ready(written) = stream.poll_write(&mut cx, &bytes) {
                    filled = written;
                }
                assert!(matches!(filled, Ok(1..)), "{filled:?}");
                let found = poll_fn(|cx| stream.poll_read(cx, &mut [0; 8])).await;
                assert_eq!(found, Err(timed_out.clone()));
                let written = poll_fn(|cx| stream.poll_write(cx, &[])).await;
                assert_eq!(written, Err(timed_out.clone()));
                let closed = poll_fn(|cx| stream.poll_close(cx)).await;
                assert_eq!(closed, Err(timed_out));
            })
        })
        .expect("the probes time out in the bound");
        drop(server);
    }

    /// With the reset's error already taken by a read on another descriptor, the
    /// kernel's `ENOTCONN` is all the close can give.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_close_after_a_reset_whose_error_was_read_gives_not_connected() {
        let (client, server) = create_pair();
        let other = client.try_clone().unwrap();
        let mut stream = stream(client);
        sockopt::set_socket_linger(&server, Some(Duration::ZERO)).unwrap();
        drop(server);
        other.set_nonblocking(false).unwrap();
        other
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let read = other.peek(&mut [0; 1]).map_err(|e| e.raw_os_error());
        assert_eq!(read, Err(Some(Errno::CONNRESET.raw_os_error())));
        other.set_nonblocking(true).unwrap();
        let closed = on_runtime(|| poll_fn(|cx| stream.poll_close(cx)));
        let code = Errno::NOTCONN.raw_os_error();
        assert_eq!(closed, Err(Error::Io { code }));
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
