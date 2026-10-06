//! An `env` stream as `hyper` I/O.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};

use env::net::{self, Tcp};
use hyper::rt::{Read, ReadBufCursor, Write};

/// The most bytes one read copies into `hyper`'s buffer.
const READ_MAX: usize = 8192;

/// A TCP stream that `hyper` reads and writes. Its errors carry the `env` error, so
/// the client can give it back.
pub(super) struct Stream(pub(super) Tcp);

fn io(error: net::Error) -> io::Error {
    io::Error::other(error)
}

impl Read for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let mut bytes = [0; READ_MAX];
        let Some(bytes) = bytes.get_mut(..buf.remaining().min(READ_MAX)) else {
            unreachable!("the length is at most READ_MAX")
        };
        if bytes.is_empty() {
            return Poll::Ready(Ok(()));
        }
        self.get_mut().0.poll_read(cx, bytes).map(|read| {
            let n = read.map_err(io)?;
            buf.put_slice(bytes.get(..n).expect("a read fits its buffer"));
            Ok(())
        })
    }
}

impl Write for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[IoSlice::new(buf)])
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_write(cx, bufs).map_err(io)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().0.poll_close(cx).map_err(io)
    }
}
