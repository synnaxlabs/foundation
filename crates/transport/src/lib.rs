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
pub mod client;
mod code;
pub mod datagram;
mod dial;
mod error;
#[cfg(feature = "fuzzing")]
pub mod fuzzing;
mod message;
pub mod port;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the datagrams of `Session` are the next users (#68)"
    )
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
#[cfg_attr(
    not(feature = "fuzzing"),
    expect(unreachable_pub, reason = "only the fuzzing feature exports it")
)]
mod tls;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the QUIC carrier is the first user")
)]
mod varint;

use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use types::ed25519::{PrivateKey, PublicKey};
use types::time::Span;

pub use address::Address;
pub use class::Class;
pub use client::Client;
pub use code::Code;
pub use error::Error;
pub use port::Port;
pub use session::{Peer, Session};

/// Ethernet's 1500 bytes less the IPv4 and UDP headers: the largest datagram this
/// node takes.
const PAYLOAD_IPV4: u16 = 1472;

/// The smallest `message_bytes_max` of either side: a datagram fits in one message,
/// and so does a hub head or key.
const MESSAGE_BYTES_MIN: usize = PAYLOAD_IPV4 as usize;

/// The rule that a pool breaks when its largest block is below [`MESSAGE_BYTES_MIN`].
const POOL_RULE: &str = "must hold a message of at least 1472 bytes";

/// The sessions of one shard. It dials peers and accepts the sessions the node
/// routes to this shard. It stays on the thread that made it. `node` binds one
/// [`Port`] and splits it into one part for each shard.
///
/// Dropping it closes each session that no caller accepted with `Code(0)`, and the
/// sessions it gave stay open. It refuses each dial from a peer until each of its
/// connections drained: each session ended, and each handshake in flight finished
/// or timed out. Then it frees its [`port::Part`], so a later dial gets no answer.
pub struct Transport {
    carrier: quic::Carrier,
    public_key: PublicKey,
}

impl Transport {
    /// Starts this shard's transport on `part`, the shard's part of the node's
    /// [`Port`].
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `config.idle` is not positive, the message limit (the
    /// smaller of `config.message_bytes_max` and `config.pool.largest()`) is below
    /// 1472, the largest UDP payload a node takes, or `config.window_bytes` is below
    /// that limit.
    ///
    /// ```
    /// use transport::{Config, Error, Transport, port};
    ///
    /// fn start(config: Config, part: port::Part) -> Result<Transport, Error> {
    ///     Transport::new(config, part)
    /// }
    /// ```
    pub fn new(config: Config, part: port::Part) -> Result<Self, Error> {
        let public_key = config.private_key.public();
        Ok(Self {
            carrier: quic::Carrier::new(config.setup()?, part),
            public_key,
        })
    }

    /// The public key that this transport proves to each peer.
    ///
    /// ```
    /// fn key(transport: &transport::Transport) -> types::ed25519::PublicKey {
    ///     transport.public_key()
    /// }
    /// ```
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }

    /// Connects to `peer` at one of `addresses`, and checks that the peer holds
    /// `peer`'s private key. It tries direct UDP addresses first, then direct TCP,
    /// then relays. It starts the next address 250 ms after the newest attempt
    /// started, or at once when it fails, and keeps the first session that completes
    /// (RFC 8305). An address where some other key answers counts as a failure,
    /// because addresses can be stale.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] when the socket is broken, or breaks before an attempt
    /// connects, or [`Error::Unreachable`] with the cause at each address when none
    /// gives a session. A session that connected before a break is given, and ends
    /// with [`Error::Network`].
    ///
    /// ```
    /// use std::net::SocketAddr;
    ///
    /// use transport::{Address, Error, Session, Transport};
    /// use types::ed25519::PublicKey;
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
        let dialed = dial::dial(&self.carrier, peer, addresses).await;
        dialed.map(Session::new)
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
        self.carrier.accept().await.map(Session::new)
    }

    /// What this transport counted since [`Transport::new`].
    ///
    /// ```
    /// fn refusals(transport: &transport::Transport) -> u64 {
    ///     transport.status().refusals
    /// }
    /// ```
    #[must_use]
    pub fn status(&self) -> Status {
        self.carrier.status()
    }
}

/// What a [`Transport`] counted since [`Transport::new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The time that at least one stream read waited for a block from the shard's
    /// pool, up to the call.
    pub waited: Span,
    /// The block commits that the system refused.
    pub refusals: u64,
    /// The sends that waited for room in the send budget of their session, which
    /// the peer's window bounds.
    pub budget_waits: u64,
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transport").finish_non_exhaustive()
    }
}

/// The inputs of a [`Transport`].
///
/// ```
/// use std::num::{NonZeroU32, NonZeroUsize};
/// use std::rc::Rc;
///
/// use transport::Config;
/// use types::ed25519::PrivateKey;
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
    /// The largest message this node accepts on a stream, and the largest datagram,
    /// at most `pool.largest()`: the transport takes the smaller of the two. Peers
    /// exchange their limits in the handshake, and each sender checks the peer's.
    pub message_bytes_max: NonZeroUsize,
    /// The most bytes in flight per session in each direction: sent and not yet
    /// acknowledged, or received and not yet taken. It bounds the memory of a session.
    /// Size it near bandwidth times round trip. Must be at least the message limit:
    /// the smaller of `message_bytes_max` and `pool.largest()`.
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
    /// The node's setup, or the first rule of [`Transport::new`] that this config
    /// breaks. A field's own range comes before its relation to another field, so the
    /// error names the field to change.
    pub(crate) fn setup(self) -> Result<quic::Setup, Error> {
        let limit = self.message_bytes_max.get().min(self.pool.largest());
        let (field, rule) = if self.idle <= Span::ZERO {
            ("idle", "must be positive")
        } else if limit < MESSAGE_BYTES_MIN {
            if limit < self.message_bytes_max.get() {
                ("pool", POOL_RULE)
            } else {
                ("message_bytes_max", "must be at least 1472")
            }
        } else if self.window_bytes < limit {
            ("window_bytes", "must be at least the message limit")
        } else {
            return Ok(quic::Setup {
                role: quic::Role::Node(self.private_key),
                message_bytes_max: limit,
                window_bytes: self.window_bytes,
                streams_max: self.streams_max,
                idle: self.idle,
                clock: self.clock,
                entropy: self.entropy,
                tasks: self.tasks,
                pool: self.pool,
            });
        };
        Err(Error::Config { field, rule })
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::rc::Rc;

    use block::{Heap, Pool};
    use types::ed25519::PrivateKey;
    use types::time::Span;

    use super::{Config, Error, Transport};
    use crate::testing::{self, Shard};
    use crate::{Address, Class, Code, Peer, Port};

    const CLIENT: PrivateKey = PrivateKey([1; 32]);
    const SERVER: PrivateKey = PrivateKey([2; 32]);

    const IDLE: Error = Error::Config {
        field: "idle",
        rule: "must be positive",
    };
    const FLOOR: Error = Error::Config {
        field: "message_bytes_max",
        rule: "must be at least 1472",
    };
    const POOL: Error = Error::Config {
        field: "pool",
        rule: "must hold a message of at least 1472 bytes",
    };
    const WINDOW: Error = Error::Config {
        field: "window_bytes",
        rule: "must be at least the message limit",
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
                let new = Transport::new(config, shard.part());
                let shown = new.map(|transport| format!("{transport:?}"));
                assert_eq!(shown, Ok("Transport { .. }".into()), "{message} bytes");
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
                (Span::SECOND, message - 1, message, WINDOW),
                (Span::SECOND, largest - 1, largest + 1, WINDOW),
            ] {
                let config = config(shard, idle, window, message);
                assert_eq!(
                    Transport::new(config, shard.part()).err(),
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
            ] {
                let config = config(shard, idle, window, message);
                assert_eq!(
                    Transport::new(config, shard.part()).err(),
                    Some(error),
                    "idle {idle:?}, window {window}, message {message}"
                );
            }
        });
    }

    #[test]
    fn new_takes_a_message_limit_over_the_pool_and_a_window_of_the_pool() {
        testing::run(0, |shard| {
            let largest = largest(shard);
            for message in [largest + 1, usize::MAX] {
                let config = config(shard, Span::SECOND, largest, message);
                let new = Transport::new(config, shard.part());
                assert_eq!(new.err(), None, "{message} bytes");
            }
        });
    }

    #[test]
    fn new_names_the_pool_when_its_largest_block_is_below_the_floor() {
        testing::run(0, |shard| {
            let budget = block::Config { budget: 1 << 10 };
            let memory = Heap::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let largest = pool.largest();
            assert!(largest < 1000, "{largest} bytes");
            for (idle, window, message, error) in [
                (Span::SECOND, 1 << 16, 1000, POOL),
                (Span::SECOND, 1 << 16, 1472, POOL),
                (Span::SECOND, 0, 1472, POOL),
                (Span::SECOND, 1 << 16, largest, FLOOR),
                (Span::ZERO, 1 << 16, 1472, IDLE),
            ] {
                let mut config = config(shard, idle, window, message);
                config.pool = Rc::clone(&pool);
                assert_eq!(
                    Transport::new(config, shard.part()).err(),
                    Some(error),
                    "idle {idle:?}, window {window}, message {message}"
                );
            }
        });
    }

    #[test]
    fn a_peer_sees_the_message_limit_of_the_pool() {
        let (mut sim, _, _) = testing::sessions(
            0,
            |config| Config {
                message_bytes_max: NonZeroUsize::MAX,
                window_bytes: config.pool.largest(),
                ..config
            },
            // Both sides have pools of the same budget.
            |side| async move {
                let opened = side.session.open_sender(Class::Command).await;
                let sender = opened.expect("a stream");
                assert_eq!(sender.bytes_max(), side.pool.largest());
            },
            |side| async move {
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_refused_config_frees_the_socket() {
        testing::run(0, |shard| {
            let at = SocketAddr::new(shard.ip(), testing::PORT);
            let config = config(shard, Span::ZERO, 1 << 16, 1 << 16);
            let new = Transport::new(config, testing::part(shard.net(), at));
            assert_eq!(new.err(), Some(IDLE));
            assert_eq!(Port::bind(shard.net(), at).err(), None);
        });
    }

    #[test]
    fn a_transport_proves_its_public_key_to_each_peer() {
        let (mut sim, _, node) = testing::nodes(0);
        testing::shard(&node, SERVER, |config, node| async move {
            let at = [
                testing::address(&node),
                SocketAddr::new(node.addresses()[0], 1),
            ];
            let server = Config {
                private_key: SERVER,
                clock: config.clock.clone(),
                entropy: config.entropy.clone(),
                tasks: config.tasks.clone(),
                pool: Rc::clone(&config.pool),
                ..config
            };
            let client = Config {
                private_key: CLIENT,
                ..config
            };
            let [a, b] = [(client, at[0]), (server, at[1])].map(|(config, at)| {
                Transport::new(config, testing::part(&node.net(), at))
                    .expect("a transport")
            });
            let addresses = [Address::Udp(at[1])];
            let dial = a.dial(b.public_key(), &addresses);
            let (accepted, dialed) = testing::join(b.accept(), dial).await;
            let accepted = accepted.expect("a session");
            assert_eq!(
                dialed.expect("a session").peer(),
                Peer::Node(b.public_key())
            );
            assert_eq!(accepted.peer(), Peer::Node(a.public_key()));
            assert_ne!(a.public_key(), b.public_key());
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn accept_gives_the_session_a_peer_dialed() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = testing::address(&server);
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            let peer = Peer::Node(CLIENT.public());
            assert_eq!(session.peer(), peer);
            assert_eq!(
                format!("{session:?}"),
                format!("Session {{ peer: {peer:?}, .. }}")
            );
            assert!(!session.relayed());
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            assert_eq!(session.peer(), Peer::Node(SERVER.public()));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_the_transport_drops_is_refused() {
        let (mut sim, client, server) = testing::nodes(0);
        let late = sim.node(sim::node::Config::default());
        let at = testing::address(&server);
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            drop(transport);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(Span::SECOND).await;
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&late, CLIENT, move |carrier, node| async move {
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            let dialed = carrier.connect(SERVER.public(), at).await;
            let reason =
                "aborted by peer: the server refused to accept a new connection";
            let reason = String::from(reason);
            assert_eq!(dialed.err(), Some(Error::Broken { reason }));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_each_session_ended_gets_no_answer() {
        let (mut sim, client, server) = testing::nodes(0);
        let late = sim.node(sim::node::Config::default());
        let at = testing::address(&server);
        testing::transport(&server, SERVER, |transport, node| async move {
            let session = transport.accept().await.expect("a session");
            drop(transport);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
            node.clock().sleep(testing::spans(testing::IDLE, 4)).await;
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&late, CLIENT, move |carrier, node| async move {
            node.clock().sleep(testing::spans(testing::IDLE, 3)).await;
            let dialed = carrier.connect(SERVER.public(), at).await;
            assert_eq!(dialed.err(), Some(Error::TimedOut));
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
