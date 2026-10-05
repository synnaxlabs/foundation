//! The driver of one TCP [`Listener`](super::Listener).

use std::net::SocketAddr;
use std::task::{Context, Poll};

use super::{Error, tcp};

/// What `os` and `sim` implement to run one [`Listener`](super::Listener). Only they
/// implement it.
///
/// It panics on a thread other than the one of the first poll.
///
/// ```
/// fn local(listener: &dyn env::net::listener::Driver) -> std::net::SocketAddr {
///     listener.local()
/// }
/// ```
pub trait Driver: Send {
    /// The local address of the listener.
    fn local(&self) -> SocketAddr;

    /// Accepts the next stream, with the rules of
    /// [`Listener::poll_accept`](super::Listener::poll_accept). An error leaves the
    /// listener usable. The stream has the options of [`tcp::Listen::options`], and no
    /// thread yet.
    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Box<dyn tcp::Driver>, Error>>;
}
