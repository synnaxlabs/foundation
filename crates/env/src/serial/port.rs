//! The driver of one [`Port`](super::Port).

use std::task::{Context, Poll};

use super::Error;

/// What `os` and `sim` implement to run one [`Port`](super::Port). Only they
/// implement it. It follows the thread rules of `Port`.
///
/// ```
/// use std::task::{Context, Poll};
///
/// /// A port with no line: no byte arrives, and each write goes nowhere.
/// struct Open;
///
/// impl env::serial::port::Driver for Open {
///     fn poll_read(
///         &mut self,
///         _: &mut Context<'_>,
///         _: &mut [u8],
///     ) -> Poll<Result<usize, env::serial::Error>> {
///         Poll::Pending
///     }
///
///     fn poll_write(
///         &mut self,
///         _: &mut Context<'_>,
///         bytes: &[u8],
///     ) -> Poll<Result<usize, env::serial::Error>> {
///         Poll::Ready(Ok(bytes.len()))
///     }
/// }
/// ```
pub trait Driver: Send {
    /// Reads bytes, with the rules of [`Port::poll_read`](super::Port::poll_read).
    /// `buffer` is not empty.
    ///
    /// # Errors
    ///
    /// As `Port::poll_read`.
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>>;

    /// Queues bytes, with the rules of
    /// [`Port::poll_write`](super::Port::poll_write). `bytes` is not empty.
    ///
    /// # Errors
    ///
    /// As `Port::poll_write`.
    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, Error>>;
}
