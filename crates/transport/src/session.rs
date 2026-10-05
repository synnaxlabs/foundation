use std::marker::PhantomData;
use std::rc::Rc;

use types::node::PublicKey;

use crate::class::Class;
use crate::code::Code;
use crate::datagram;
use crate::error::Error;
use crate::stream::{Incoming, Receiver, Sender};

/// A connection to one peer. It carries streams of whole messages and datagrams that
/// may drop. It stays on the shard that made it.
///
/// Clones share the session, and so do its streams and datagram halves. When the
/// last of them drops, the session closes with `Code(0)` once the peer has every
/// finished stream, or after [`Config::idle`](crate::Config::idle). The session also
/// ends when either side closes it, or when its carrier fails.
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
        todo!("#68")
    }

    /// Whether the session runs through a relay node. It never changes: a session is
    /// direct or relayed as a whole, and a better path means a new session. A relayed
    /// round trip is not symmetric, so it does not measure clock offset well.
    ///
    /// ```
    /// fn may_sync_clocks(session: &transport::Session) -> bool {
    ///     !session.relayed()
    /// }
    /// ```
    #[must_use]
    pub fn relayed(&self) -> bool {
        todo!("#68")
    }

    /// Opens a stream in both directions. It waits while the peer allows no more
    /// streams; dropping the future before it completes opens nothing. The peer sees
    /// the stream at its first message or finish.
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
        todo!("#68")
    }

    /// Opens a stream that only this node sends on. It waits while the peer allows no
    /// more streams; dropping the future before it completes opens nothing. The peer
    /// sees the stream at its first message or finish.
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
        todo!("#68")
    }

    /// Waits for the next stream the peer opened, highest class first. Every clone
    /// shares one queue, and each stream goes to one caller, so one dispatcher per
    /// session should take them.
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
        todo!("#68")
    }

    /// The session's datagrams. Every call gives halves of the same queues.
    ///
    /// ```
    /// use transport::{Session, datagram};
    ///
    /// fn split(session: &Session) -> (datagram::Sender, datagram::Receiver) {
    ///     session.datagrams()
    /// }
    /// ```
    #[must_use]
    pub fn datagrams(&self) -> (datagram::Sender, datagram::Receiver) {
        todo!("#68")
    }

    /// Closes the session with `code` now. Data not yet delivered drops, streams on
    /// it end, and the peer sees [`Error::PeerClosed`]. It does not wait. Closing an
    /// ended session does nothing.
    ///
    /// ```
    /// fn leave(session: &transport::Session) {
    ///     session.close(transport::Code(0));
    /// }
    /// ```
    pub fn close(&self, code: Code) {
        let _ = code;
        todo!("#68")
    }

    /// Waits until the session ends and returns why.
    ///
    /// ```
    /// async fn watch(session: &transport::Session) -> transport::Error {
    ///     session.closed().await
    /// }
    /// ```
    pub async fn closed(&self) -> Error {
        todo!("#68")
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
