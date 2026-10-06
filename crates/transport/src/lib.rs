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
#[cfg(test)]
mod testing;
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

/// Ethernet's 1500 bytes less the IPv4 and UDP headers: the largest datagram this
/// node takes.
const PAYLOAD_IPV4: u16 = 1472;

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
    /// below `config.message_bytes_max`, or `config.message_bytes_max` is below 1472
    /// or over `config.pool.largest()`.
    ///
    /// ```
    /// use transport::{Config, Error, Transport};
    ///
    /// fn start(config: Config) -> Result<Transport, Error> {
    ///     Transport::new(config)
    /// }
    /// ```
    pub fn new(config: Config) -> Result<Self, Error> {
        config.check()?;
        drop(config);
        Ok(Self {
            _shard: PhantomData,
        })
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
    /// peer's. Must be at least 1472, the largest UDP payload a node takes, and at
    /// most `pool.largest()`.
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
    /// Every random value the carriers use outside TLS: the connection IDs and the
    /// random generator of each QUIC endpoint. TLS draws its own.
    pub entropy: env::entropy::Entropy,
    /// Spawns the tasks that drive each carrier on this shard.
    pub tasks: env::tasks::Tasks,
    /// The shard's pool. Each received message lands in one block from it.
    pub pool: Rc<block::Pool>,
}

impl Config {
    /// The first rule of [`Transport::new`] that this config breaks. A field's own
    /// range comes before its relation to another field, so the error names the field
    /// to change.
    fn check(&self) -> Result<(), Error> {
        let message_bytes_max = self.message_bytes_max.get();
        let (field, rule) = if self.idle <= Span::ZERO {
            ("idle", "must be positive")
        } else if message_bytes_max < usize::from(PAYLOAD_IPV4) {
            ("message_bytes_max", "must be at least 1472")
        } else if message_bytes_max > self.pool.largest() {
            ("message_bytes_max", "must be at most pool.largest()")
        } else if self.window_bytes < message_bytes_max {
            ("window_bytes", "must be at least message_bytes_max")
        } else {
            return Ok(());
        };
        Err(Error::Config { field, rule })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::rc::Rc;

    use block::{Heap, Pool};
    use types::node::PrivateKey;
    use types::time::Span;

    use super::{Config, Error, Transport};
    use crate::testing::{self, Shard};

    const IDLE: Error = Error::Config {
        field: "idle",
        rule: "must be positive",
    };
    const FLOOR: Error = Error::Config {
        field: "message_bytes_max",
        rule: "must be at least 1472",
    };
    const CEILING: Error = Error::Config {
        field: "message_bytes_max",
        rule: "must be at most pool.largest()",
    };
    const WINDOW: Error = Error::Config {
        field: "window_bytes",
        rule: "must be at least message_bytes_max",
    };

    /// A config of `shard` with these limits.
    fn config(shard: &Shard, idle: Span, window: usize, message: usize) -> Config {
        let mut config = shard.config(PrivateKey([1; 32]), idle);
        config.window_bytes = window;
        config.message_bytes_max = NonZeroUsize::new(message).expect("not zero");
        config
    }

    /// The most bytes one block of the pool of `shard` holds.
    fn largest(shard: &Shard) -> usize {
        shard
            .config(PrivateKey([1; 32]), Span::SECOND)
            .pool
            .largest()
    }

    #[test]
    fn new_takes_each_limit_at_its_edge() {
        testing::run(0, |shard| {
            for message in [1472, largest(shard)] {
                let config = config(shard, Span::NANOSECOND, message, message);
                assert_eq!(Transport::new(config).err(), None, "{message} bytes");
            }
        });
    }

    #[test]
    fn new_refuses_each_limit_just_past_its_edge() {
        testing::run(0, |shard| {
            let largest = largest(shard);
            let message = 1 << 16;
            for (idle, window, message, error) in [
                (Span::ZERO, message, message, IDLE),
                (Span::from_nanos(-1), message, message, IDLE),
                (Span::SECOND, 1471, 1471, FLOOR),
                (Span::SECOND, largest + 1, largest + 1, CEILING),
                (Span::SECOND, message - 1, message, WINDOW),
            ] {
                let config = config(shard, idle, window, message);
                assert_eq!(
                    Transport::new(config).err(),
                    Some(error),
                    "idle {idle:?}, window {window}, message {message}"
                );
            }
        });
    }

    #[test]
    fn new_gives_the_earlier_of_two_broken_rules() {
        testing::run(0, |shard| {
            let largest = largest(shard);
            for (idle, window, message, error) in [
                (Span::ZERO, 1471, 1471, IDLE),
                (Span::ZERO, largest + 1, largest + 1, IDLE),
                (Span::ZERO, 0, 1 << 16, IDLE),
                (Span::SECOND, 0, 1471, FLOOR),
                (Span::SECOND, 0, largest + 1, CEILING),
            ] {
                let config = config(shard, idle, window, message);
                assert_eq!(
                    Transport::new(config).err(),
                    Some(error),
                    "idle {idle:?}, window {window}, message {message}"
                );
            }
        });
    }

    #[test]
    fn new_gives_the_floor_before_the_ceiling_of_a_small_pool() {
        testing::run(0, |shard| {
            let budget = block::Config { budget: 1 << 10 };
            let memory = Heap::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            assert!(pool.largest() < 1000, "{} bytes", pool.largest());
            for (message, error) in [(1000, FLOOR), (1472, CEILING)] {
                let mut config = config(shard, Span::SECOND, message, message);
                config.pool = Rc::clone(&pool);
                assert_eq!(
                    Transport::new(config).err(),
                    Some(error),
                    "{message} bytes"
                );
            }
        });
    }
}
