//! The driver of the [`Receiver`](super::Receiver).

use std::io::IoSliceMut;
use std::task::{Context, Poll};

use super::Meta;
use crate::net::Error;

/// What `os` and `sim` implement to run a [`Receiver`](super::Receiver). Only they
/// implement it.
///
/// It panics on a thread other than the one of the first poll.
///
/// ```
/// use std::io::IoSliceMut;
/// use std::task::{Context, Poll};
///
/// use env::net::udp::{Meta, receiver};
///
/// fn receive(
///     driver: &mut dyn receiver::Driver,
///     cx: &mut Context<'_>,
///     buffers: &mut [IoSliceMut<'_>],
///     meta: &mut [Meta],
/// ) -> Poll<Result<usize, env::net::Error>> {
///     driver.poll_recv(cx, buffers, meta)
/// }
/// ```
pub trait Driver: Send {
    /// Receives, with the rules of
    /// [`Receiver::poll_recv`](super::Receiver::poll_recv). It absorbs the errors of
    /// one datagram and gives an error only when the socket is broken.
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>>;
}
