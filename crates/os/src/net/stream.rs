//! A TCP stream: the kernel's socket, polled through Tokio.

use std::io::IoSlice;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use env::net::{Error, tcp};
use rustix::io::Errno;
use rustix::net::{Shutdown, sockopt};
#[cfg(not(target_os = "macos"))]
use tokio::io::AsyncWrite;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::TcpStream;

use super::socket::Socket;
#[cfg(target_os = "macos")]
use super::unsent;
use super::{apply, errno, io_error, stream_error};

/// A connected stream. `SO_LINGER` 0 is set from `new` until `poll_close`, so a drop
/// before it resets the peer.
pub(super) struct Stream {
    socket: Socket<std::net::TcpStream, TcpStream>,
    local: SocketAddr,
    peer: SocketAddr,
    /// The kernel does not apply the bound to a write on macOS.
    #[cfg(target_os = "macos")]
    unsent: unsent::Bound,
    /// `poll_close` ran: the FIN is queued.
    closed: bool,
    /// The error that ended the stream. The kernel reports a reset once and then an
    /// end of stream, so each later write of bytes, close, and read that finds no
    /// bytes gives this.
    failed: Option<Error>,
}

impl Stream {
    /// A stream over `stream`, connected from `local` to `peer`, before its first poll.
    /// It sets `options` and `SO_LINGER` 0, unless `failed` already ended the stream.
    ///
    /// # Errors
    ///
    /// The code of a failed option, unless the error that ended the socket caused it.
    pub(super) fn new(
        stream: std::net::TcpStream,
        local: SocketAddr,
        peer: SocketAddr,
        options: &tcp::Options,
        failed: Option<Error>,
    ) -> Result<Self, Errno> {
        let failed = match failed {
            Some(failed) => Some(failed),
            None => match apply(stream.as_fd(), options).and_then(|()| {
                sockopt::set_socket_linger(&stream, Some(Duration::ZERO))
            }) {
                Ok(()) => None,
                Err(code) => match Self::ended(&stream, code) {
                    Some(ended) => Some(stream_error(ended, peer)),
                    None => return Err(code),
                },
            },
        };
        Ok(Self {
            socket: Socket::new(stream),
            local,
            peer,
            #[cfg(target_os = "macos")]
            unsent: unsent::Bound::new(options.unsent_bytes_max),
            closed: false,
            failed,
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
            .live("TCP stream", TcpStream::from_std)
            .map_err(io_error)?;
        Ok(Pin::new(stream))
    }

    /// The error that ended `stream`, when a call on it gave `code` because the
    /// connection ended with no poll that reported why: `ENOTCONN` from a shutdown,
    /// and on macOS `EINVAL` from an option after a reset.
    fn ended(stream: impl AsFd, code: Errno) -> Option<Errno> {
        match code {
            Errno::NOTCONN | Errno::INVAL => Self::pending(stream),
            _ => None,
        }
    }

    /// The error the kernel holds for the stream, after an event no poll reported.
    fn pending(stream: impl AsFd) -> Option<Errno> {
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
        // The kernel takes at most 1024 parts, and a write of only empty parts gives 0
        // with the stream still ready, so a caller's loop would spin.
        let skip = buffers
            .iter()
            .take_while(|buffer| buffer.is_empty())
            .count();
        let buffers = &buffers[skip..];
        if buffers.is_empty() {
            self.socket.bind("TCP stream");
            return Poll::Ready(Ok(0));
        }
        let peer = self.peer;
        let stream = Self::live(&mut self.socket)?;
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        if self.closed {
            // A reset after the close is the stream's end. Without one, the write
            // is a misuse, as `sim` reports it.
            return Poll::Ready(Err(match Self::pending(&*stream) {
                Some(code) => self.fail(stream_error(code, peer)),
                None => io_error(Errno::PIPE),
            }));
        }
        #[cfg(not(target_os = "macos"))]
        let sent =
            ready!(stream.poll_write_vectored(cx, buffers)).map_err(|e| errno(&e));
        #[cfg(target_os = "macos")]
        let sent = ready!(self.unsent.send(&stream, cx, buffers));
        match sent {
            Ok(written) => Poll::Ready(Ok(written)),
            Err(code) => Poll::Ready(Err(self.fail(stream_error(code, peer)))),
        }
    }

    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let peer = self.peer;
        let stream = Self::live(&mut self.socket)?;
        if let Some(failed) = &self.failed {
            return Poll::Ready(Err(failed.clone()));
        }
        if self.closed {
            return Poll::Ready(match Self::pending(&*stream) {
                Some(code) => Err(self.fail(stream_error(code, peer))),
                None => Ok(()),
            });
        }
        // The linger goes first: macOS refuses an option on a socket shut both ways.
        let shut = sockopt::set_socket_linger(&*stream, None)
            .and_then(|()| rustix::net::shutdown(&*stream, Shutdown::Write))
            .map_err(|code| Self::ended(&*stream, code).unwrap_or(code));
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
    use std::num::NonZeroUsize;
    use std::os::fd::OwnedFd;

    use tcp::Driver as _;

    use super::*;

    fn options() -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
            delayed: true,
        }
    }

    /// A connected pair on the loopback, with the blocking calls of std.
    fn create_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        create_pair_with(|_| {})
    }

    /// A connected pair whose listener `set` sets options on before the connect.
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    fn create_pair_with(
        set: impl FnOnce(&TcpListener),
    ) -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        set(&listener);
        let client =
            std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn stream(socket: std::net::TcpStream) -> Stream {
        let peer = socket.peer_addr().unwrap();
        stream_to(socket, peer)
    }

    /// A stream over `socket` that names `peer`, which the kernel may no longer hold.
    fn stream_to(socket: std::net::TcpStream, peer: SocketAddr) -> Stream {
        socket.set_nonblocking(true).unwrap();
        let local = socket.local_addr().unwrap();
        Stream::new(socket, local, peer, &options(), None).unwrap()
    }

    /// An address to name a peer the kernel no longer holds.
    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 1)
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
            remote: stream.peer(),
        };
        let bound = Duration::from_secs(10);
        on_runtime(|| {
            tokio::time::timeout(bound, async {
                let block = vec![0; 1 << 16];
                let bytes = [IoSlice::new(&block)];
                // The first poll registers the socket and is pending.
                let mut filled = poll_fn(|cx| stream.poll_write(cx, &bytes)).await;
                let mut cx = Context::from_waker(std::task::Waker::noop());
                while let Poll::Ready(written) = stream.poll_write(&mut cx, &bytes) {
                    filled = written;
                }
                assert!(matches!(filled, Ok(1..)), "{filled:?}");
                let found = poll_fn(|cx| stream.poll_read(cx, &mut [0; 8])).await;
                assert_eq!(found, Err(timed_out.clone()));
                let written = poll_fn(|cx| stream.poll_write(cx, &bytes)).await;
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

    /// macOS refuses the linger with `EINVAL`, and the stream takes the reset from
    /// the socket. Linux sets it, and the read finds the reset.
    #[test]
    fn a_stream_over_a_socket_a_reset_ended_reads_the_reset() {
        let (client, server) = create_pair();
        sockopt::set_socket_linger(&server, Some(Duration::ZERO)).unwrap();
        drop(server);
        let mut tries = 0;
        while client.peer_addr().is_ok() {
            tries += 1;
            assert!(tries < 1 << 20, "the reset arrives");
            std::thread::yield_now();
        }
        let mut stream = stream_to(client, loopback());
        let reset = Err(Error::Reset { remote: loopback() });
        let read = on_runtime(|| poll_fn(|cx| stream.poll_read(cx, &mut [0; 8])));
        assert_eq!(read, reset);
    }

    /// Only `ENOTCONN` and `EINVAL` say the connection ended. Another code keeps
    /// the pending error for the poll that reports it. No public call fails with
    /// another code after a reset, so this calls `ended`. The `INVAL` call reads and
    /// clears the pending error, so it comes last.
    #[test]
    fn only_not_connected_and_invalid_read_the_pending_error() {
        let (client, server) = create_pair();
        sockopt::set_socket_linger(&server, Some(Duration::ZERO)).unwrap();
        drop(server);
        let mut tries = 0;
        while client.peer_addr().is_ok() {
            tries += 1;
            assert!(tries < 1 << 20, "the reset arrives");
            std::thread::yield_now();
        }
        assert_eq!(Stream::ended(&client, Errno::BADF), None);
        assert_eq!(Stream::ended(&client, Errno::AGAIN), None);
        assert_eq!(Stream::ended(&client, Errno::INVAL), Some(Errno::CONNRESET));
    }

    /// Writes blocks of `part` bytes to a stream with `options` whose peer reads
    /// nothing, until a write waits. Gives the bytes the stream's socket holds, and
    /// its segment size.
    #[cfg(target_os = "macos")]
    fn held_at_the_stall(options: &tcp::Options, part: usize) -> (usize, usize) {
        let (client, server) = create_pair_with(|listener| {
            sockopt::set_socket_recv_buffer_size(listener, 1 << 14).unwrap();
        });
        client.set_nonblocking(true).unwrap();
        let segment = super::super::lowat::segment(client.as_fd()).unwrap();
        let (local, peer) = (client.local_addr().unwrap(), client.peer_addr().unwrap());
        let mut stream = Stream::new(client, local, peer, options, None).unwrap();
        let block = vec![7; part];
        let bytes = [IoSlice::new(&block)];
        let written = on_runtime(|| async {
            let mut written = 0;
            let bound = Duration::from_millis(250);
            while let Ok(sent) =
                tokio::time::timeout(bound, poll_fn(|cx| stream.poll_write(cx, &bytes)))
                    .await
            {
                written += sent.unwrap();
            }
            written
        });
        let received = rustix::io::ioctl_fionread(&server).unwrap();
        (
            written - usize::try_from(received).unwrap(),
            usize::try_from(segment).unwrap(),
        )
    }

    /// XNU posts the write event at the bound, so one more write of the bound fills
    /// it twice.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_unsent_bytes_stay_at_most_twice_the_bound() {
        let options = tcp::Options {
            send_buffer_bytes: 1 << 20,
            delayed: false,
            ..options()
        };
        let (held, _) = held_at_the_stall(&options, 1 << 20);
        assert!(held <= 2 * options.unsent_bytes_max.get(), "{held}");
    }

    /// With `delayed`, XNU also posts the write event under one segment, whatever
    /// the bound.
    #[test]
    #[cfg(target_os = "macos")]
    fn delayed_unsent_bytes_stay_at_most_the_bound_plus_one_segment() {
        let options = tcp::Options {
            send_buffer_bytes: 1 << 20,
            unsent_bytes_max: NonZeroUsize::new(1 << 13).unwrap(),
            delayed: true,
            ..options()
        };
        let (held, segment) = held_at_the_stall(&options, 64);
        let max = options.unsent_bytes_max.get();
        assert!(segment > max, "the bound is below one segment: {segment}");
        assert!(held <= max + segment, "{held}, segment {segment}");
    }

    #[test]
    fn new_sets_linger_zero() {
        let (client, _server) = create_pair();
        let stream = stream(client);
        // The public tests see the reset; this one names the option that makes it.
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
