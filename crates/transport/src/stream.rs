//! The two halves of a stream: an ordered, reliable sequence of whole messages.

use std::marker::PhantomData;
use std::rc::Rc;

use block::Block;

use crate::class::Class;
use crate::code::Code;
use crate::error::Error;

/// The sending half of a stream. Dropping it finishes the stream.
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
        todo!()
    }

    /// Sends `message` whole. It waits while the peer's flow control has no room,
    /// and returns once the stream holds the message, not when the peer has it.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over
    /// [`Config::message_bytes_max`](crate::Config::message_bytes_max),
    /// [`Error::Stopped`] when the peer stopped reading, or the error that ended the
    /// session.
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// async fn send(sender: &mut Sender, frame: block::Block) -> Result<(), Error> {
    ///     sender.send(frame).await
    /// }
    /// ```
    pub async fn send(&mut self, message: Block) -> Result<(), Error> {
        let _ = message;
        todo!()
    }

    /// Ends the stream after the messages already sent. The peer's
    /// [`Receiver::recv`] returns `None` after the last one.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped reading, or the error that ended the
    /// session.
    ///
    /// ```
    /// fn done(sender: transport::stream::Sender) -> Result<(), transport::Error> {
    ///     sender.finish()
    /// }
    /// ```
    pub fn finish(self) -> Result<(), Error> {
        todo!()
    }

    /// Cancels the stream: messages not yet delivered drop, and the peer sees
    /// [`Error::Reset`] with `code`.
    ///
    /// ```
    /// fn cancel(sender: transport::stream::Sender) {
    ///     sender.reset(transport::Code(1));
    /// }
    /// ```
    pub fn reset(self, code: Code) {
        let _ = code;
        todo!()
    }
}

/// The receiving half of a stream. Dropping it before the end stops the stream with
/// `Code(0)`.
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
    /// [`Error::Reset`] when the sender cancelled the stream, or the error that ended
    /// the session.
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
        todo!()
    }

    /// Asks the sender to stop: messages not yet received drop, and the sender sees
    /// [`Error::Stopped`] with `code`.
    ///
    /// ```
    /// fn hang_up(receiver: transport::stream::Receiver) {
    ///     receiver.stop(transport::Code(1));
    /// }
    /// ```
    pub fn stop(self, code: Code) {
        let _ = code;
        todo!()
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
