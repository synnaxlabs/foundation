//! Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS
//! over TCP, relays, and diodes; never calls up.
//!
//! Every carrier but the one-way diode serves one session model; the diode gets its
//! own surface. A [`Session`] connects this node to one peer. It carries streams of
//! whole messages, each with a traffic [`Class`] that sets its priority and the
//! carrier it prefers, and datagrams that may drop. A carrier that lacks a feature
//! emulates it, so no caller sees which carrier it uses.
//!
//! Each shard owns one [`Transport`] and the sessions it makes. Nothing here calls up:
//! callers pull sessions with [`Transport::accept`], decide which peers to admit, and
//! route each stream to its protocol.
//!
//! A complete reader on `hub` asks a remote home for frames and receives them in
//! order:
//!
//! ```
//! use block::Block;
//! use transport::{Class, Error, Session};
//!
//! async fn read(session: &Session, request: Block) -> Result<(), Error> {
//!     let (mut sender, mut receiver) = session.open(Class::Complete).await?;
//!     sender.send(request).await?;
//!     sender.finish()?;
//!     while let Some(frame) = receiver.recv().await? {
//!         let _ = frame.len();
//!     }
//!     Ok(())
//! }
//! ```

mod address;
mod class;
mod code;
pub mod datagram;
mod error;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the QUIC carrier is the first user")
)]
mod message;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "`Transport::new` is the first user")
)]
mod quic;
mod session;
pub mod stream;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the TCP and QUIC carriers are the first users")
)]
mod tls;

use std::marker::PhantomData;
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use types::node::{PrivateKey, PublicKey};
use types::time::Span;

pub use address::Address;
pub use class::Class;
pub use code::Code;
pub use error::Error;
pub use session::{Peer, Session};

/// The sessions of one shard. It dials peers and accepts the sessions the node
/// routes to this shard. It stays on the thread that made it. The node's sockets and
/// relays belong to one node-level part that every shard shares (#77).
#[derive(Debug)]
pub struct Transport {
    _shard: PhantomData<Rc<()>>,
}

impl Transport {
    /// Starts this shard's part of the transport.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `config.idle` is not positive, `config.window_bytes` is
    /// below `config.message_bytes_max`, or `config.message_bytes_max` is over
    /// `config.pool.largest()`.
    ///
    /// ```
    /// use transport::{Config, Error, Transport};
    ///
    /// fn start(config: Config) -> Result<Transport, Error> {
    ///     Transport::new(config)
    /// }
    /// ```
    pub fn new(config: Config) -> Result<Self, Error> {
        drop(config);
        todo!("#68")
    }

    /// Connects to `peer` at one of `addresses`, and checks that the peer holds
    /// `peer`'s private key. It tries direct UDP addresses first, then direct TCP,
    /// then relays. It starts the next address when the current one fails or has not
    /// answered after a short stagger, and keeps the first session that completes
    /// (RFC 8305). An address where some other key answers counts as a failure,
    /// because addresses can be stale.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] with the cause at each address when none gives a
    /// session.
    ///
    /// ```
    /// use std::net::SocketAddr;
    ///
    /// use transport::{Address, Error, Session, Transport};
    /// use types::node::PublicKey;
    ///
    /// async fn dial(t: &Transport, peer: PublicKey, at: SocketAddr)
    /// -> Result<Session, Error> {
    ///     t.dial(peer, &[Address::Udp(at), Address::Tcp(at)]).await
    /// }
    /// ```
    pub async fn dial(
        &self,
        peer: PublicKey,
        addresses: &[Address],
    ) -> Result<Session, Error> {
        let _ = (peer, addresses);
        todo!("#68")
    }

    /// Waits for the next session that a peer opened and the node routed to this
    /// shard. The peer has completed the handshake; the caller decides whether to
    /// admit it and closes it if not. Handshakes that fail never reach the caller.
    ///
    /// # Errors
    ///
    /// The error that stopped this shard's part of the transport.
    ///
    /// ```
    /// use transport::{Error, Transport};
    ///
    /// async fn serve(transport: &Transport) -> Result<(), Error> {
    ///     loop {
    ///         let session = transport.accept().await?;
    ///         let _ = session.peer();
    ///     }
    /// }
    /// ```
    pub async fn accept(&self) -> Result<Session, Error> {
        todo!("#68")
    }
}

/// The inputs of a [`Transport`].
///
/// ```
/// use std::num::{NonZeroU32, NonZeroUsize};
/// use std::rc::Rc;
///
/// use transport::Config;
/// use types::node::PrivateKey;
/// use types::time::Span;
///
/// fn config(
///     clock: env::clock::Clock,
///     entropy: env::entropy::Entropy,
///     tasks: env::tasks::Tasks,
///     pool: Rc<block::Pool>,
/// ) -> Config {
///     Config {
///         private_key: PrivateKey([7; 32]),
///         message_bytes_max: NonZeroUsize::new(16 << 20).expect("not zero"),
///         window_bytes: 32 << 20,
///         streams_max: NonZeroU32::new(1_024).expect("not zero"),
///         idle: Span::MINUTE,
///         clock,
///         entropy,
///         tasks,
///         pool,
///     }
/// }
/// ```
#[derive(Debug)]
pub struct Config {
    /// The node's key. Peers authenticate the node by its public key.
    pub private_key: PrivateKey,
    /// The largest message this node accepts on a stream, and the largest datagram.
    /// Peers exchange their limits in the handshake, and each sender checks the
    /// peer's. Must be at most `pool.largest()`.
    pub message_bytes_max: NonZeroUsize,
    /// The most bytes in flight per session in each direction: sent and not yet
    /// acknowledged, or received and not yet taken. It bounds the memory of a session.
    /// Size it near bandwidth times round trip. Must be at least `message_bytes_max`.
    pub window_bytes: usize,
    /// The most two-way streams, and apart from them the most one-way streams, a peer
    /// may have open to this node at once, per session. Size it near the rate of new
    /// streams times the time each takes to deliver.
    pub streams_max: NonZeroU32,
    /// A session whose peer is silent this long ends with [`Error::TimedOut`].
    /// Sessions send keep-alives, so a live peer is never silent this long. Must be
    /// positive.
    pub idle: Span,
    /// The monotonic clock for timeouts, pacing, and keep-alives.
    pub clock: env::clock::Clock,
    /// Randomness for keys and nonces.
    pub entropy: env::entropy::Entropy,
    /// Spawns the tasks that drive each carrier on this shard.
    pub tasks: env::tasks::Tasks,
    /// The shard's pool. Each received message lands in one block from it.
    pub pool: Rc<block::Pool>,
}
