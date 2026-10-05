//! TCP stream settings, and the driver of one [`Tcp`](super::Tcp) stream.

use std::io::IoSlice;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use super::Error;

/// The socket options of one TCP stream.
///
/// ```
/// let options = env::net::tcp::Options {
///     send_buffer_bytes: 1 << 20,
///     recv_buffer_bytes: 1 << 20,
///     unsent_bytes_max: 1 << 14,
///     delayed: false,
/// };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The size of the OS send buffer.
    pub send_buffer_bytes: usize,
    /// The size of the OS receive buffer.
    pub recv_buffer_bytes: usize,
    /// The most bytes written but not yet sent before a write waits
    /// (`TCP_NOTSENT_LOWAT`).
    pub unsent_bytes_max: usize,
    /// Small writes wait to join later ones (Nagle's algorithm). Otherwise each write
    /// goes out at once.
    pub delayed: bool,
}

/// Settings for one TCP connect.
///
/// ```
/// fn dial(options: env::net::tcp::Options) -> env::net::tcp::Config {
///     env::net::tcp::Config {
///         remote: "10.0.0.2:4433".parse().expect("an address"),
///         options,
///     }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The address to connect to.
    pub remote: SocketAddr,
    /// The options of the stream.
    pub options: Options,
}

/// Settings for one TCP listener.
///
/// ```
/// fn serve(options: env::net::tcp::Options) -> env::net::tcp::Listen {
///     env::net::tcp::Listen {
///         local: "0.0.0.0:4433".parse().expect("an address"),
///         backlog: 128,
///         options,
///     }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listen {
    /// The local address. Port 0 binds a free port.
    pub local: SocketAddr,
    /// The most connections that wait for an accept.
    pub backlog: u32,
    /// The options of each accepted stream.
    pub options: Options,
}

/// What `os` and `sim` implement to run one [`Tcp`](super::Tcp) stream. Only they
/// implement it.
///
/// Each poll follows the rules of the [`Tcp`](super::Tcp) call of the same name, and
/// panics on a thread other than the one of the first poll.
///
/// ```
/// fn peer(stream: &dyn env::net::tcp::Driver) -> std::net::SocketAddr {
///     stream.peer()
/// }
/// ```
pub trait Driver: Send {
    /// The local address of the stream.
    fn local(&self) -> SocketAddr;

    /// The address of the peer.
    fn peer(&self) -> SocketAddr;

    /// Reads into `buffer`.
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>>;

    /// Writes from `buffers` as one vectored write.
    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>>;

    /// Closes this side for writing.
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>>;
}
