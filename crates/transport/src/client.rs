//! The transport of a program that dials nodes with no node key.

use std::num::NonZeroU32;
use std::rc::Rc;

use types::ed25519::PublicKey;
use types::time::Span;

use crate::{Address, Error, MESSAGE_BYTES_MIN, POOL_RULE, Session, dial, port, quic};

/// The smallest window of a program: 1 Gbit/s over a round trip of 8 ms.
const WINDOW_BYTES_MIN: usize = 1 << 20;

/// The idle timeout of a program. Keep-alives go out well inside it, so only a
/// session whose node is gone ends.
const IDLE: Span = Span::from_nanos(30 * Span::SECOND.nanos());

/// A program's sessions to nodes. It dials a node with no node key, so the node sees
/// it as [`Peer::Client`](crate::Peer::Client). It accepts no session: it answers a
/// dial with a stateless reset, which the dialer ignores until the dial times out. It
/// stays on the thread that made it.
///
/// Dropping it closes nothing that it gave. Each session stays open until its last
/// clone drops. Once each connection drained, it frees its [`port::Part`].
pub struct Client {
    carrier: quic::Carrier,
}

impl Client {
    /// Starts a program's transport on `part`. Bind a [`Port`](crate::Port) at port 0
    /// and give it one part. The limits are fixed: messages up to `pool.largest()`, a
    /// window of that or 1 MiB, whichever is larger, and a 30 s idle timeout. A node
    /// may open 1 two-way and 1 one-way stream to it at a time, though it opens none.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `config.pool.largest()` is below 1472, the largest UDP
    /// payload a node takes.
    ///
    /// ```
    /// use transport::{Client, Error, client, port};
    ///
    /// fn start(config: client::Config, part: port::Part) -> Result<Client, Error> {
    ///     Client::new(config, part)
    /// }
    /// ```
    pub fn new(config: Config, part: port::Part) -> Result<Self, Error> {
        Ok(Self {
            carrier: quic::Carrier::new(config.setup()?, part),
        })
    }

    /// Connects to `node` at one of `addresses`, and checks that the node holds
    /// `node`'s private key. The address order, the stagger, and the errors are those
    /// of [`Transport::dial`](crate::Transport::dial). The session's peer is
    /// [`Peer::Node`](crate::Peer::Node) with `node`.
    ///
    /// # Errors
    ///
    /// As [`Transport::dial`](crate::Transport::dial).
    ///
    /// ```
    /// use std::net::SocketAddr;
    ///
    /// use transport::{Address, Client, Error, Session};
    /// use types::ed25519::PublicKey;
    ///
    /// async fn dial(c: &Client, node: PublicKey, at: SocketAddr)
    /// -> Result<Session, Error> {
    ///     c.dial(node, &[Address::Udp(at)]).await
    /// }
    /// ```
    pub async fn dial(
        &self,
        node: PublicKey,
        addresses: &[Address],
    ) -> Result<Session, Error> {
        let dialed = dial::dial(&self.carrier, node, addresses).await;
        dialed.map(Session::new)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// The inputs of a [`Client`]: only what a program must give.
///
/// ```
/// use std::rc::Rc;
///
/// use transport::client::Config;
///
/// fn config(
///     clock: env::clock::Clock,
///     entropy: env::entropy::Entropy,
///     tasks: env::tasks::Tasks,
///     pool: Rc<block::Pool>,
/// ) -> Config {
///     Config { clock, entropy, tasks, pool }
/// }
/// ```
#[derive(Debug)]
pub struct Config {
    /// The monotonic clock for timeouts, pacing, and keep-alives.
    pub clock: env::clock::Clock,
    /// Every random value the carrier uses outside TLS.
    pub entropy: env::entropy::Entropy,
    /// Spawns the task that drives the carrier.
    pub tasks: env::tasks::Tasks,
    /// Each received message lands in one block from it.
    pub pool: Rc<block::Pool>,
}

impl Config {
    /// The program's setup, or the rule of [`Client::new`] that this config breaks.
    pub(crate) fn setup(self) -> Result<quic::Setup, Error> {
        let message_bytes_max = self.pool.largest();
        if message_bytes_max < MESSAGE_BYTES_MIN {
            return Err(Error::Config {
                field: "pool",
                rule: POOL_RULE,
            });
        }
        Ok(quic::Setup {
            role: quic::Role::Program,
            message_bytes_max,
            window_bytes: message_bytes_max.max(WINDOW_BYTES_MIN),
            streams_max: NonZeroU32::MIN,
            idle: IDLE,
            clock: self.clock,
            entropy: self.entropy,
            tasks: self.tasks,
            pool: self.pool,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use block::{Heap, Pool};
    use sim::node::Node;
    use types::ed25519::PrivateKey;

    use super::*;
    use crate::testing::{self, IDLE, Shard, address, nodes, spans};
    use crate::{Class, Code, Peer};

    const SERVER: PrivateKey = PrivateKey([2; 32]);
    const OTHER: PrivateKey = PrivateKey([3; 32]);

    /// Starts a transport for `SERVER` on `node` that accepts `sessions` sessions in
    /// turn. On each, it checks that the peer is a program, echoes the first message
    /// of the first stream, and waits until the program drops the session.
    fn serve(node: &Node, sessions: usize) {
        testing::shard(node, SERVER, move |config, node| async move {
            // A program's pool has the budget of this one.
            let largest = config.pool.largest();
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = crate::Transport::new(config, part).expect("a transport");
            for _ in 0..sessions {
                let session = transport.accept().await.expect("a session");
                assert_eq!(session.peer(), Peer::Client);
                let mut incoming = session.accept().await.expect("a stream");
                let received = incoming.receiver.recv().await.expect("a message");
                let mut sender = incoming.sender.expect("a two-way stream");
                assert_eq!(sender.bytes_max(), largest);
                sender.send(received.expect("one")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(session.closed().await, closed);
            }
        });
    }

    /// Starts a transport for `OTHER` on `node` that lives for 3 idle timeouts.
    fn impostor(node: &Node) {
        testing::transport(node, OTHER, |transport, node| async move {
            node.clock().sleep(spans(IDLE, 3)).await;
            drop(transport);
        });
    }

    /// A client on `shard` bound at `at`.
    fn bind(shard: &Shard, at: SocketAddr) -> Client {
        let part = testing::part(shard.net(), at);
        Client::new(shard.client(), part).expect("a client")
    }

    /// Sends a message to the node of `session` on a two-way stream and checks the
    /// echo.
    async fn echo(shard: &Shard, session: &Session) {
        let (mut sender, mut receiver) =
            session.open(Class::Command).await.expect("a stream");
        sender.send(shard.block(b"ping")).await.expect("sent");
        sender.finish().expect("finished");
        let echoed = receiver.recv().await.expect("a message").expect("one");
        assert_eq!(&echoed[..], b"ping");
    }

    /// Drops `session` and gives its close time to go out, because a shard that ends
    /// drops its tasks.
    async fn end(node: &Node, session: Session) {
        drop(session);
        node.clock().sleep(Span::MILLISECOND).await;
    }

    #[test]
    fn a_client_dials_a_node_that_sees_a_program() {
        let (mut sim, program, node) = nodes(0);
        serve(&node, 1);
        let at = [Address::Udp(address(&node))];
        testing::start(&program, move |shard, node| async move {
            let client = bind(&shard, address(&node));
            let session = client.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(SERVER.public()));
            assert_eq!(format!("{client:?}"), "Client { .. }");
            echo(&shard, &session).await;
            end(&node, session).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_client_skips_an_address_where_another_key_answers() {
        let (mut sim, program, node) = nodes(0);
        serve(&node, 1);
        let other = sim.node(sim::node::Config::default());
        impostor(&other);
        let at = [Address::Udp(address(&other)), Address::Udp(address(&node))];
        testing::start(&program, move |shard, node| async move {
            let client = bind(&shard, address(&node));
            let session = client.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(SERVER.public()));
            echo(&shard, &session).await;
            end(&node, session).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_client_where_another_key_answers_at_each_address_is_unreachable() {
        let (mut sim, program, _) = nodes(0);
        let others = [(); 2].map(|()| sim.node(sim::node::Config::default()));
        others.iter().for_each(impostor);
        let at = others.each_ref().map(|other| Address::Udp(address(other)));
        testing::start(&program, move |shard, node| async move {
            let client = bind(&shard, address(&node));
            let peer = SERVER.public();
            let dialed = client.dial(peer, &at).await;
            let cause = || Error::Authentication { expected: peer };
            let attempts = vec![(at[0], cause()), (at[1], cause())];
            assert_eq!(dialed.err(), Some(Error::Unreachable { peer, attempts }));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_to_a_client_times_out() {
        let (mut sim, program, node) = nodes(0);
        let at = Address::Udp(address(&program));
        testing::start(&program, |shard, node| async move {
            let client = bind(&shard, address(&node));
            node.clock().sleep(spans(IDLE, 3)).await;
            drop(client);
        });
        // The client answers the Initial with a stateless reset, which the dialer
        // ignores, so the handshake times out.
        testing::transport(&node, SERVER, move |transport, _| async move {
            let peer = OTHER.public();
            let dialed = transport.dial(peer, &[at]).await;
            let attempts = vec![(at, Error::TimedOut)];
            assert_eq!(dialed.err(), Some(Error::Unreachable { peer, attempts }));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_client_keeps_its_sessions_and_frees_its_part_after_them() {
        let (mut sim, program, node) = nodes(0);
        serve(&node, 2);
        let at = [Address::Udp(address(&node))];
        testing::start(&program, move |shard, node| async move {
            let client = bind(&shard, address(&node));
            let session = client.dial(SERVER.public(), &at).await.expect("a session");
            drop(client);
            node.clock().sleep(spans(IDLE, 3)).await;
            echo(&shard, &session).await;
            end(&node, session).await;
            node.clock().sleep(spans(IDLE, 3)).await;
            let client = bind(&shard, address(&node));
            let session = client.dial(SERVER.public(), &at).await.expect("a session");
            echo(&shard, &session).await;
            end(&node, session).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// Reads the setup: each field reaches the endpoint by the path of a node, which
    /// the tests of `Transport` pin.
    #[test]
    fn a_program_has_the_fixed_limits() {
        testing::run(0, |shard| {
            let budget = block::Config { budget: 1 << 16 };
            let memory = Heap::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let largest = pool.largest();
            assert!(largest < 1 << 20, "{largest} bytes");
            let config = Config {
                pool,
                ..shard.client()
            };
            let setup = config.setup().expect("a setup");
            assert!(matches!(setup.role, quic::Role::Program));
            assert_eq!(setup.message_bytes_max, largest);
            assert_eq!(setup.window_bytes, 1 << 20);
            assert_eq!(setup.streams_max, NonZeroU32::MIN);
            assert_eq!(setup.idle, Span::from_nanos(30_000_000_000));
        });
    }

    #[test]
    fn new_names_the_pool_when_its_largest_block_is_below_the_floor() {
        testing::run(0, |shard| {
            let budget = block::Config { budget: 1 << 10 };
            let memory = Heap::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let config = Config {
                pool,
                ..shard.client()
            };
            let error = Error::Config {
                field: "pool",
                rule: "must hold a message of at least 1472 bytes",
            };
            assert_eq!(Client::new(config, shard.part()).err(), Some(error));
        });
    }
}
