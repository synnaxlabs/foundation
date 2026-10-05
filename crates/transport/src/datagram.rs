//! The two halves of a session's datagrams: whole messages that may be lost or arrive
//! out of order.

use std::marker::PhantomData;
use std::rc::Rc;

use block::Block;

use crate::error::Error;

/// Sends datagrams. Clones share the session's send queue.
///
/// ```
/// use transport::{Error, datagram::Sender};
///
/// fn push(sender: &Sender, frame: block::Block) -> Result<bool, Error> {
///     if frame.len() > sender.bytes_max() {
///         return Ok(false);
///     }
///     sender.send(frame)?;
///     Ok(true)
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Sender {
    _shard: PhantomData<Rc<()>>,
}

impl Sender {
    /// Sends `message` as one datagram. It never waits: when the link is busy, the
    /// oldest unsent datagram drops.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over [`bytes_max`](Self::bytes_max), or
    /// the error that ended the session.
    ///
    /// ```
    /// use transport::{Error, datagram::Sender};
    ///
    /// fn send(sender: &Sender, sample: block::Block) -> Result<(), Error> {
    ///     sender.send(sample)
    /// }
    /// ```
    pub fn send(&self, message: Block) -> Result<(), Error> {
        drop(message);
        todo!("#68")
    }

    /// The largest datagram the session sends now. It changes with the path and the
    /// carrier. It is never over the peer's `message_bytes_max`, and never so large
    /// that a datagram cannot drop.
    ///
    /// ```
    /// fn fits(sender: &transport::datagram::Sender, frame: &block::Block) -> bool {
    ///     frame.len() <= sender.bytes_max()
    /// }
    /// ```
    #[must_use]
    pub fn bytes_max(&self) -> usize {
        todo!("#68")
    }
}

/// Receives datagrams. Every receiver of a session shares one queue, and each
/// datagram goes to one of them.
///
/// ```
/// use transport::{Error, datagram::Receiver};
///
/// async fn drain(mut receiver: Receiver) -> Error {
///     loop {
///         if let Err(error) = receiver.recv().await {
///             return error;
///         }
///     }
/// }
/// ```
#[derive(Debug)]
pub struct Receiver {
    _shard: PhantomData<Rc<()>>,
}

impl Receiver {
    /// Waits for the next datagram. It lands in one block from the shard's pool.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when the pool has no room for the next datagram, or the error
    /// that ended the session.
    ///
    /// ```
    /// use block::Block;
    /// use transport::{Error, datagram::Receiver};
    ///
    /// async fn next(receiver: &mut Receiver) -> Result<Block, Error> {
    ///     receiver.recv().await
    /// }
    /// ```
    pub async fn recv(&mut self) -> Result<Block, Error> {
        todo!("#68")
    }
}
