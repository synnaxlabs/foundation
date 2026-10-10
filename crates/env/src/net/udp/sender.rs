//! The driver of one [`Sender`](super::Sender) clone.

use std::task::{Context, Poll};

use super::Transmit;
use crate::net::Error;

/// What `os` and `sim` implement to run one [`Sender`](super::Sender) clone. Only they
/// implement it.
///
/// It panics on a thread other than the one of the first poll.
///
/// ```
/// use std::task::{Context, Poll};
///
/// use env::net::udp::{Transmit, sender};
///
/// fn send(
///     driver: &mut dyn sender::Driver,
///     cx: &mut Context<'_>,
///     transmit: &Transmit<'_>,
/// ) -> Poll<Result<(), env::net::Error>> {
///     driver.poll_send(cx, transmit)
/// }
/// ```
pub trait Driver: Send {
    /// Sends `transmit`, with the rules of
    /// [`Sender::poll_send`](super::Sender::poll_send). An error affects only this
    /// transmit.
    fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>>;
}
