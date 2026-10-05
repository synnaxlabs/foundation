use std::marker::PhantomData;
use std::rc::Rc;

use block::Block;
use types::node::PublicKey;

use crate::class::Class;
use crate::code::Code;
use crate::error::Error;
use crate::stream::{Incoming, Receiver, Sender};

/// A connection to one peer. It carries streams of whole messages and datagrams that
/// may drop. Clones share the session; dropping the last one closes it with
/// `Code(0)`. It stays on the shard that made it.
///
/// ```
/// use block::Block;
/// use transport::{Class, Error, Session};
///
/// async fn ask(session: &Session, question: Block) -> Result<Option<Block>, Error> {
///     let (mut sender, mut receiver) = session.open(Class::Command).await?;
///     sender.send(question).await?;
///     sender.finish()?;
///     receiver.recv().await
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Session {
    _shard: PhantomData<Rc<()>>,
}

impl Session {
    /// Who is on the other end.
    ///
    /// ```
    /// use transport::{Peer, Session};
    ///
    /// fn is_node(session: &Session) -> bool {
    ///     matches!(session.peer(), Peer::Node(_))
    /// }
    /// ```
    #[must_use]
    pub fn peer(&self) -> Peer {
        todo!()
    }

    /// Whether the session runs through a relay node. A relayed round trip is not
    /// symmetric, so it does not measure clock offset well.
    ///
    /// ```
    /// fn may_sync_clocks(session: &transport::Session) -> bool {
    ///     !session.relayed()
    /// }
    /// ```
    #[must_use]
    pub fn relayed(&self) -> bool {
        todo!()
    }

    /// Opens a stream in both directions. It waits while the peer allows no more
    /// streams.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::stream::{Receiver, Sender};
    /// use transport::{Class, Error, Session};
    ///
    /// async fn open(session: &Session) -> Result<(Sender, Receiver), Error> {
    ///     session.open(Class::Complete).await
    /// }
    /// ```
    pub async fn open(&self, class: Class) -> Result<(Sender, Receiver), Error> {
        let _ = class;
        todo!()
    }

    /// Opens a stream that only this node sends on. It waits while the peer allows no
    /// more streams.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::{Class, Error, Session, stream};
    ///
    /// async fn open(session: &Session) -> Result<stream::Sender, Error> {
    ///     session.open_sender(Class::Latest).await
    /// }
    /// ```
    pub async fn open_sender(&self, class: Class) -> Result<Sender, Error> {
        let _ = class;
        todo!()
    }

    /// Waits for the next stream the peer opened.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::{Error, Session};
    ///
    /// async fn serve(session: &Session) -> Result<(), Error> {
    ///     loop {
    ///         let incoming = session.accept().await?;
    ///         let _ = incoming.class;
    ///     }
    /// }
    /// ```
    pub async fn accept(&self) -> Result<Incoming, Error> {
        todo!()
    }

    /// Sends `message` as one datagram. It never waits: when the link is busy, the
    /// oldest unsent datagram drops. A datagram may also be lost or arrive out of
    /// order.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over
    /// [`datagram_bytes_max`](Self::datagram_bytes_max), or the error that ended the
    /// session.
    ///
    /// ```
    /// use transport::{Error, Session};
    ///
    /// fn send(session: &Session, sample: block::Block) -> Result<(), Error> {
    ///     session.send_datagram(sample)
    /// }
    /// ```
    #[expect(clippy::needless_pass_by_value, reason = "stub until implemented")]
    pub fn send_datagram(&self, message: Block) -> Result<(), Error> {
        let _ = message;
        todo!()
    }

    /// Waits for the next datagram.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::{Error, Session};
    ///
    /// async fn recv(session: &Session) -> Result<block::Block, Error> {
    ///     session.recv_datagram().await
    /// }
    /// ```
    pub async fn recv_datagram(&self) -> Result<Block, Error> {
        todo!()
    }

    /// The largest datagram the session sends now. It changes with the path, and a
    /// carrier with no datagrams of its own emulates them up to the message limit.
    ///
    /// ```
    /// fn fits(session: &transport::Session, frame: &block::Block) -> bool {
    ///     frame.len() <= session.datagram_bytes_max()
    /// }
    /// ```
    #[must_use]
    pub fn datagram_bytes_max(&self) -> usize {
        todo!()
    }

    /// Closes the session with `code`. Streams on it end, and the peer sees
    /// [`Error::PeerClosed`]. It does not wait.
    ///
    /// ```
    /// fn leave(session: &transport::Session) {
    ///     session.close(transport::Code(0));
    /// }
    /// ```
    pub fn close(&self, code: Code) {
        let _ = code;
        todo!()
    }

    /// Waits until the session ends and returns why.
    ///
    /// ```
    /// async fn watch(session: &transport::Session) -> transport::Error {
    ///     session.closed().await
    /// }
    /// ```
    pub async fn closed(&self) -> Error {
        todo!()
    }
}

/// Who is on the other end of a [`Session`].
///
/// ```
/// use transport::Peer;
///
/// fn key(peer: Peer) -> Option<types::node::PublicKey> {
///     match peer {
///         Peer::Node(key) => Some(key),
///         Peer::Client => None,
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    /// A node that proved it holds this key.
    Node(PublicKey),
    /// A program with no node key, such as an SDK. It proves who it is above the
    /// transport, with a signed hello.
    Client,
}
