//! The two halves of a stream: an ordered, reliable sequence of whole messages.

use std::marker::PhantomData;
use std::rc::Rc;

use block::Block;

use crate::class::Class;
use crate::code::Code;
use crate::error::Error;

/// The sending half of a stream. Dropping it without [`finish`](Self::finish) resets
/// the stream with `Code(0)`, so the peer never reads a cut-off stream as complete.
/// Dropping it after `finish` lets delivery go on.
///
/// ```
/// use block::Block;
/// use transport::{Error, stream::Sender};
///
/// async fn send_all(mut sender: Sender, frames: Vec<Block>) -> Result<(), Error> {
///     for frame in frames {
///         sender.send(frame).await?;
///     }
///     sender.finish()
/// }
/// ```
#[derive(Debug)]
pub struct Sender {
    _shard: PhantomData<Rc<()>>,
}

impl Sender {
    /// The stream's traffic class.
    ///
    /// ```
    /// fn class(sender: &transport::stream::Sender) -> transport::Class {
    ///     sender.class()
    /// }
    /// ```
    #[must_use]
    pub fn class(&self) -> Class {
        todo!("#68")
    }

    /// Sends `message` whole. It waits while the peer's flow control has no room,
    /// and returns once the stream holds the message, not when the peer has it. If the
    /// future drops before it completes, the stream resets with `Code(0)`, because
    /// part of the message may be sent.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over the peer's
    /// [`Config::message_bytes_max`](crate::Config::message_bytes_max),
    /// [`Error::Stopped`] when the peer stopped reading, or the error that ended the
    /// session.
    ///
    /// # Panics
    ///
    /// When called after [`finish`](Self::finish).
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// async fn send(sender: &mut Sender, frame: block::Block) -> Result<(), Error> {
    ///     sender.send(frame).await
    /// }
    /// ```
    pub async fn send(&mut self, message: Block) -> Result<(), Error> {
        drop(message);
        todo!("#68")
    }

    /// Ends the stream after the messages already sent. The peer's
    /// [`Receiver::recv`] returns `None` after the last one. The sender stays, so
    /// [`reset`](Self::reset) can still cancel what the peer does not have yet.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped reading, or the error that ended the
    /// session.
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// fn done(sender: &mut Sender) -> Result<(), Error> {
    ///     sender.finish()
    /// }
    /// ```
    pub fn finish(&mut self) -> Result<(), Error> {
        todo!("#68")
    }

    /// Cancels the stream: messages the peer does not have yet drop, and the peer
    /// sees [`Error::Reset`] with `code`. After [`finish`](Self::finish), it does
    /// nothing once the peer has every message.
    ///
    /// ```
    /// fn cancel(sender: transport::stream::Sender) {
    ///     sender.reset(transport::Code(16));
    /// }
    /// ```
    pub fn reset(self, code: Code) {
        let _ = code;
        todo!("#68")
    }
}

/// The receiving half of a stream. Dropping it before the end stops the stream with
/// `Code(0)`; dropping it after the end sends nothing.
///
/// ```
/// use transport::{Error, stream::Receiver};
///
/// async fn drain(mut receiver: Receiver) -> Result<usize, Error> {
///     let mut count = 0;
///     while receiver.recv().await?.is_some() {
///         count += 1;
///     }
///     Ok(count)
/// }
/// ```
#[derive(Debug)]
pub struct Receiver {
    _shard: PhantomData<Rc<()>>,
}

impl Receiver {
    /// Waits for the next whole message. It lands in one block from the shard's pool.
    /// Returns `None` once the sender finished and every message has arrived.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the sender cancelled the stream, [`Error::Pool`] when the
    /// pool has no room for the next message (it stays queued), or the error that
    /// ended the session.
    ///
    /// ```
    /// use block::Block;
    /// use transport::{Error, stream::Receiver};
    ///
    /// async fn next(receiver: &mut Receiver) -> Result<Option<Block>, Error> {
    ///     receiver.recv().await
    /// }
    /// ```
    pub async fn recv(&mut self) -> Result<Option<Block>, Error> {
        todo!("#68")
    }

    /// Asks the sender to stop: messages not yet received drop, and the sender sees
    /// [`Error::Stopped`] with `code`.
    ///
    /// ```
    /// fn hang_up(receiver: transport::stream::Receiver) {
    ///     receiver.stop(transport::Code(16));
    /// }
    /// ```
    pub fn stop(self, code: Code) {
        let _ = code;
        todo!("#68")
    }
}

/// A stream the peer opened. It has a sender when the peer opened it in both
/// directions with [`Session::open`](crate::Session::open), and none when it used
/// [`Session::open_sender`](crate::Session::open_sender). The sender has the same
/// class.
///
/// ```
/// use transport::stream::Incoming;
///
/// fn answerable(incoming: &Incoming) -> bool {
///     incoming.sender.is_some()
/// }
/// ```
#[derive(Debug)]
pub struct Incoming {
    /// The class the peer opened it with.
    pub class: Class,
    /// Messages from the peer.
    pub receiver: Receiver,
    /// Messages to the peer, when the stream goes both ways.
    pub sender: Option<Sender>,
}
