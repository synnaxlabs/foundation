//! Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS
//! over TCP, relays, and diodes; never calls up.
//!
//! Every carrier serves one session model. A [`Session`] connects this node to one
//! peer. It carries streams of whole messages, each with a traffic [`Class`] that sets
//! its priority and the carrier it prefers, and datagrams that may drop. A carrier
//! that lacks a feature emulates it, so no caller sees which carrier it uses.
//!
//! Each shard owns one [`Transport`] and the sessions it makes. Nothing here calls up:
//! callers pull sessions with [`Transport::accept`] and decide which peers to admit.
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
mod error;
mod identity;
mod session;
pub mod stream;

use std::marker::PhantomData;
use std::rc::Rc;

use types::node::PublicKey;
use types::time::Span;

pub use address::Address;
pub use class::Class;
pub use code::Code;
pub use error::Error;
pub use identity::Identity;
pub use session::{Peer, Session};

/// The sessions of one shard. It binds the shard's sockets, dials peers, and accepts
/// sessions. It stays on the thread that made it.
#[derive(Debug)]
pub struct Transport {
    _shard: PhantomData<Rc<()>>,
}

impl Transport {
    /// Binds every address in `config.listen` and joins its relays.
    ///
    /// # Errors
    ///
    /// None yet. The network seam adds the errors of binding a socket.
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
        todo!()
    }

    /// The addresses this transport listens on, with the ports the network gave it.
    ///
    /// ```
    /// fn publish(transport: &transport::Transport) -> Vec<transport::Address> {
    ///     transport.addresses().to_vec()
    /// }
    /// ```
    #[must_use]
    pub fn addresses(&self) -> &[Address] {
        todo!()
    }

    /// Connects to `peer` at one of `addresses`. It tries direct UDP first, then
    /// direct TCP, then each relay, and checks that the peer holds `peer`'s private
    /// key.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] when no address answers, and
    /// [`Error::Authentication`] when the peer cannot prove the key.
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
        todo!()
    }

    /// Waits for the next session that a peer opened. The peer has completed the
    /// handshake; the caller decides whether to admit it and closes it if not.
    /// Handshakes that fail never reach the caller.
    ///
    /// ```
    /// async fn serve(transport: &transport::Transport) {
    ///     loop {
    ///         let session = transport.accept().await;
    ///         let _ = session.peer();
    ///     }
    /// }
    /// ```
    pub async fn accept(&self) -> Session {
        todo!()
    }
}

/// The inputs of a [`Transport`].
///
/// ```
/// use std::rc::Rc;
///
/// use transport::{Address, Config, Identity};
/// use types::time::Span;
///
/// fn config(
///     clock: env::clock::Clock,
///     entropy: env::entropy::Entropy,
///     tasks: env::tasks::Tasks,
///     pool: Rc<block::Pool>,
/// ) -> Config {
///     let at = "0.0.0.0:7400".parse().expect("a valid address");
///     Config {
///         identity: Identity::new([7; 32]),
///         listen: vec![Address::Udp(at), Address::Tcp(at)],
///         message_bytes_max: 16 << 20,
///         streams_max: 1_024,
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
    /// The node's key pair.
    pub identity: Identity,
    /// Where to accept sessions: local UDP and TCP sockets to bind, and relays to stay
    /// joined to. Port 0 asks the network for a free port.
    pub listen: Vec<Address>,
    /// The largest message a stream or datagram carries, in either direction.
    pub message_bytes_max: usize,
    /// The most streams a peer may have open to this node at once, per session.
    pub streams_max: u32,
    /// A session whose peer is silent this long ends with [`Error::TimedOut`].
    /// Sessions send keep-alives, so a live peer is never silent this long.
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
