//! The unsent bound on macOS, whose kernel applies `TCP_NOTSENT_LOWAT` only to the
//! write event, not to the write itself.

use std::io;
use std::io::IoSlice;
use std::task::{Context, Poll, ready};

use rustix::io::Errno;
use tokio::io::Interest;
use tokio::net::TcpStream;

use super::errno;

/// The bytes a stream wrote since the count last reached the bound.
pub(super) struct Bound {
    max: usize,
    /// Below `max`.
    written: usize,
}

impl Bound {
    /// A bound of `max` unsent bytes.
    pub(super) fn new(max: usize) -> Self {
        Self {
            // A bound of 0 would write nothing, and wait for no event.
            max: max.max(1),
            written: 0,
        }
    }

    /// Writes from `buffers` to `stream`, at most the bound less the count. When the
    /// count reaches the bound, it clears the write readiness, so the next write
    /// waits for the write event, which honors the bound. The unsent bytes so stay at most twice the bound. With `delayed`, XNU
    /// also posts the event under one segment, so they stay at most the bound plus
    /// the larger of the bound and one segment.
    pub(super) fn send(
        &mut self,
        stream: &TcpStream,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Errno>> {
        let room = self.max - self.written;
        let (mut whole, mut len) = (0, 0);
        while let Some(buffer) = buffers.get(whole)
            && len + buffer.len() <= room
        {
            (whole, len) = (whole + 1, len + buffer.len());
        }
        let head;
        let buffers = match buffers.get(whole) {
            // The first part crosses the bound, so the write takes its head.
            Some(next) if whole == 0 => {
                head = [IoSlice::new(&next[..room])];
                &head[..]
            }
            _ => &buffers[..whole],
        };
        loop {
            ready!(stream.poll_write_ready(cx)).map_err(|e| errno(&e))?;
            let mut sent = Err(Errno::AGAIN);
            // `WouldBlock` from the closure clears the readiness, unless an event
            // came during the write.
            let _cleared = stream.try_io(Interest::WRITABLE, || {
                sent = rustix::io::writev(stream, buffers);
                match sent {
                    Ok(n) if self.written + n < self.max => Ok(()),
                    _ => Err(io::ErrorKind::WouldBlock.into()),
                }
            });
            match sent {
                Err(Errno::AGAIN) => {}
                Ok(n) => {
                    self.written = (self.written + n) % self.max;
                    return Poll::Ready(Ok(n));
                }
                Err(code) => return Poll::Ready(Err(code)),
            }
        }
    }
}
