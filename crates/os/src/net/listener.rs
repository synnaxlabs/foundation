//! A TCP listener: the kernel's socket, polled through Tokio.

use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::task::{Context, Poll, ready};

use env::net::{Error, listener, tcp};
use rustix::io::Errno;
use rustix::net::sockopt;
use tokio::net::{TcpListener, TcpStream};

use super::socket::Socket;
use super::stream::Stream;
use super::{apply, errno, io_error, socket};

/// A listening socket. Each accepted stream gets `options`.
pub(super) struct Listener {
    socket: Socket<std::net::TcpListener, TcpListener>,
    local: SocketAddr,
    options: tcp::Options,
}

impl Listener {
    /// Listens on `config.local`, with `SO_REUSEADDR` so a restart binds a port in
    /// `TIME_WAIT`. Needs no runtime.
    pub(super) fn listen(config: &tcp::Listen) -> Result<Self, Error> {
        let local = config.local;
        let fd = socket(local).map_err(io_error)?;
        sockopt::set_socket_reuseaddr(&fd, true).map_err(io_error)?;
        match rustix::net::bind(&fd, &local) {
            Err(Errno::ADDRINUSE) => return Err(Error::AddressInUse { local }),
            outcome => outcome.map_err(io_error)?,
        }
        let backlog = i32::try_from(config.backlog).unwrap_or(i32::MAX);
        rustix::net::listen(&fd, backlog).map_err(io_error)?;
        let listener = std::net::TcpListener::from(fd);
        let local = listener.local_addr().map_err(|e| io_error(errno(&e)))?;
        Ok(Self {
            socket: Socket::Idle(listener),
            local,
            options: config.options,
        })
    }
}

/// A stream the kernel accepted, with the options of the listener set.
fn accepted(
    stream: TcpStream,
    peer: SocketAddr,
    options: &tcp::Options,
) -> Result<Stream, Error> {
    apply(stream.as_fd(), options).map_err(io_error)?;
    let stream = stream.into_std().map_err(|e| io_error(errno(&e)))?;
    let local = stream.local_addr().map_err(|e| io_error(errno(&e)))?;
    Ok(Stream::new(stream, local, peer))
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
        loop {
            match ready!(listener.poll_accept(cx)) {
                Ok((stream, peer)) => {
                    let stream = accepted(stream, peer, &self.options)?;
                    return Poll::Ready(Ok(Box::new(stream)));
                }
                // The peer left the backlog before the accept: no stream.
                Err(e) if errno(&e) == Errno::CONNABORTED => {}
                Err(e) => return Poll::Ready(Err(io_error(errno(&e)))),
            }
        }
    }
}
