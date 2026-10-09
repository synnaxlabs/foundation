//! A sim shard for a [`Config`], and runs of two nodes with a carrier, a transport,
//! or a session on each.

use std::future::poll_fn;
use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::Poll;

use block::{Block, Heap, Pool, Unique};
use env::clock::Clock;
use env::entropy::Entropy;
use env::net::Net;
use env::tasks::Tasks;
use sim::Sim;
use sim::node::Node;
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::{Address, Config, Port, Session, Transport, client, port, quic};

/// The most streams of each kind a peer may open, in [`Shard::config`].
pub(crate) const STREAMS_MAX: u32 = 16;
/// The largest message of each side, in [`Shard::config`].
pub(crate) const MESSAGE_BYTES_MAX: usize = 1 << 16;

/// The UDP port of [`address`].
pub(crate) const PORT: u16 = 4433;

/// The key of the client node of [`sessions`].
pub(crate) const CLIENT: PrivateKey = PrivateKey([1; 32]);
/// The key of the server node of [`sessions`].
pub(crate) const SERVER: PrivateKey = PrivateKey([2; 32]);
/// The idle timeout of the configs that [`shard`] gives.
pub(crate) const IDLE: Span = Span::SECOND;

/// What one sim shard gives a [`Config`].
pub(crate) struct Shard {
    clock: Clock,
    entropy: Entropy,
    tasks: Tasks,
    /// The pool of each config.
    pool: Rc<Pool>,
    net: Net,
    /// The node's first IP.
    ip: IpAddr,
}

impl Shard {
    /// What a shard of `node` with `tasks` gives, with a pool of 2 MiB. Each node
    /// reserves about 110 MiB for it, so a test of 4 nodes on each of 8 threads stays
    /// under the 4 GiB cap of a CI test process.
    pub(crate) fn new(node: &sim::node::Node, tasks: Tasks) -> Self {
        let config = block::Config { budget: 1 << 21 };
        let memory = Heap::new(config.reservation());
        Self {
            clock: node.clock(),
            entropy: node.entropy(),
            tasks,
            pool: Rc::new(Pool::new(config, memory)),
            net: node.net(),
            ip: node.addresses()[0],
        }
    }

    pub(crate) fn net(&self) -> &Net {
        &self.net
    }

    /// The node's first IP.
    pub(crate) fn ip(&self) -> IpAddr {
        self.ip
    }

    /// The part of a port at a free port of the node.
    pub(crate) fn part(&self) -> port::Part {
        part(&self.net, SocketAddr::new(self.ip, 0))
    }

    /// A config for a node with `private_key` and `idle`, on this shard.
    pub(crate) fn config(&self, private_key: PrivateKey, idle: Span) -> Config {
        Config {
            private_key,
            message_bytes_max: NonZeroUsize::new(MESSAGE_BYTES_MAX).expect("not zero"),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(STREAMS_MAX).expect("not zero"),
            idle,
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: self.tasks.clone(),
            pool: Rc::clone(&self.pool),
        }
    }

    /// A config for a program on this shard.
    pub(crate) fn client(&self) -> client::Config {
        client::Config {
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: self.tasks.clone(),
            pool: Rc::clone(&self.pool),
        }
    }

    /// A block from [`Shard::pool`] that holds `bytes`.
    pub(crate) fn block(&self, bytes: &[u8]) -> Block {
        block(&self.pool, bytes)
    }

    /// The bytes of [`Shard::pool`] that blocks hold or keep for the next alloc.
    pub(crate) fn committed(&self) -> usize {
        self.pool.committed()
    }
}

/// A block of `len` bytes from `pool`, or `None` when it has no room.
pub(crate) fn alloc(pool: &Pool, len: usize) -> Option<Unique> {
    pool.alloc(len).ok()
}

/// A block from `pool` that holds `bytes`.
pub(crate) fn block(pool: &Pool, bytes: &[u8]) -> Block {
    let mut block = pool.alloc(bytes.len()).expect("room");
    block.copy_from_slice(bytes);
    block.freeze()
}

/// `n` times `span`.
pub(crate) fn spans(span: Span, n: i64) -> Span {
    Span::from_nanos(span.nanos() * n)
}

/// A run from `value` with a client node and a server node.
pub(crate) fn nodes(value: u64) -> (Sim, Node, Node) {
    let mut sim = Sim::new(sim::Config {
        seed: value,
        ..sim::Config::default()
    });
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    (sim, client, server)
}

/// The address at [`PORT`] on the first IP of `node`.
pub(crate) fn address(node: &Node) -> SocketAddr {
    SocketAddr::new(node.addresses()[0], PORT)
}

/// The one part of a port bound at `at`.
pub(crate) fn part(net: &Net, at: SocketAddr) -> port::Part {
    let port = Port::bind(net, at).expect("a port");
    let mut parts = port.split(NonZeroUsize::MIN);
    parts.pop().expect("one part")
}

/// Starts a shard on `node` that runs `main` with a config for `key` and [`IDLE`].
pub(crate) fn shard<F: Future<Output = ()> + 'static>(
    node: &Node,
    key: PrivateKey,
    main: impl FnOnce(Config, Node) -> F + Send + 'static,
) {
    start(node, move |shard, node| main(shard.config(key, IDLE), node));
}

/// Starts a shard on `node` that runs `main` with what the shard gives.
pub(crate) fn start<F: Future<Output = ()> + 'static>(
    node: &Node,
    main: impl FnOnce(Shard, Node) -> F + Send + 'static,
) {
    let own = node.clone();
    let config = env::shards::Config {
        name: "transport".into(),
        core: None,
    };
    let started = node.shards().start(config, move |tasks| async move {
        main(Shard::new(&own, tasks), own).await;
    });
    drop(started.expect("a shard"));
}

/// The setup of `config`.
///
/// # Panics
///
/// When [`Transport::new`] refuses `config`, with its error.
pub(crate) fn setup(config: &Config) -> quic::Setup {
    copy(config)
        .setup()
        .unwrap_or_else(|error| panic!("{error}"))
}

/// A config equal to `config`.
fn copy(config: &Config) -> Config {
    Config {
        private_key: config.private_key.clone(),
        clock: config.clock.clone(),
        entropy: config.entropy.clone(),
        tasks: config.tasks.clone(),
        pool: Rc::clone(&config.pool),
        ..*config
    }
}

/// Starts a shard on `node` that runs `main` with a QUIC carrier for `key` at
/// [`address`].
pub(crate) fn carrier<F: Future<Output = ()> + 'static>(
    node: &Node,
    key: PrivateKey,
    main: impl FnOnce(quic::Carrier, Node) -> F + Send + 'static,
) {
    shard(node, key, |config, node| async move {
        let part = part(&node.net(), address(&node));
        main(quic::Carrier::new(setup(&config), part), node).await;
    });
}

/// Starts a shard on `node` that runs `main` with a transport for `key` at
/// [`address`].
pub(crate) fn transport<F: Future<Output = ()> + 'static>(
    node: &Node,
    key: PrivateKey,
    main: impl FnOnce(Transport, Node) -> F + Send + 'static,
) {
    shard(node, key, |config, node| async move {
        let part = part(&node.net(), address(&node));
        let transport = Transport::new(config, part).expect("a transport");
        main(transport, node).await;
    });
}

/// `count` transports on `node` with the limits of `config`, each with its own key
/// and port from [`PORT`] up, so that each makes its own session to one peer.
pub(crate) fn transports(config: &Config, node: &Node, count: u8) -> Vec<Transport> {
    (0..count)
        .map(|index| {
            let config = Config {
                private_key: PrivateKey([10 + index; 32]),
                ..copy(config)
            };
            let at = SocketAddr::new(node.addresses()[0], PORT + u16::from(index));
            Transport::new(config, part(&node.net(), at)).expect("a transport")
        })
        .collect()
}

/// Runs `test` on one shard of a sim run made from `value`, and gives its result.
pub(crate) fn run<T: Send + 'static>(
    value: u64,
    test: impl FnOnce(&Shard) -> T + Send + 'static,
) -> T {
    let mut sim = sim::Sim::new(sim::Config {
        seed: value,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        test(&Shard::new(&node, tasks))
    })
    .expect("the test passes")
}

/// One end of the session of [`sessions`].
pub(crate) struct Side {
    pub(crate) session: Session,
    pub(crate) node: Node,
    pub(crate) pool: Rc<Pool>,
    pub(crate) transport: Transport,
}

impl Side {
    /// A block from the side's pool that holds `bytes`.
    pub(crate) fn block(&self, bytes: &[u8]) -> Block {
        block(&self.pool, bytes)
    }
}

/// A run from `value` with a client node and a server node. The client's transport
/// for [`CLIENT`] dials the server's for [`SERVER`], whose config `tune` gives from
/// the default, and each side runs its function on its end of the session. A side
/// that drops its session at the end sends the close.
pub(crate) fn sessions<C, S>(
    value: u64,
    tune: impl FnOnce(Config) -> Config + Send + 'static,
    client: impl FnOnce(Side) -> C + Send + 'static,
    server: impl FnOnce(Side) -> S + Send + 'static,
) -> (Sim, Node, Node)
where
    C: Future<Output = ()> + 'static,
    S: Future<Output = ()> + 'static,
{
    let (sim, client_node, server_node) = nodes(value);
    let at = address(&server_node);
    shard(&server_node, SERVER, move |config, node| async move {
        let config = tune(config);
        let pool = Rc::clone(&config.pool);
        let part = part(&node.net(), address(&node));
        let transport = Transport::new(config, part).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let clock = node.clock();
        server(Side {
            session,
            node,
            pool,
            transport,
        })
        .await;
        // A shard that ends drops its tasks, so give the close time to go out.
        clock.sleep(Span::MILLISECOND).await;
    });
    shard(&client_node, CLIENT, move |config, node| async move {
        let pool = Rc::clone(&config.pool);
        let part = part(&node.net(), address(&node));
        let transport = Transport::new(config, part).expect("a transport");
        let addresses = [Address::Udp(at)];
        let dialed = transport.dial(SERVER.public(), &addresses).await;
        let session = dialed.expect("a session");
        let clock = node.clock();
        client(Side {
            session,
            node,
            pool,
            transport,
        })
        .await;
        // A shard that ends drops its tasks, so give the close time to go out.
        clock.sleep(Span::MILLISECOND).await;
    });
    (sim, client_node, server_node)
}

/// Polls `future` once, and gives its output when it is ready.
pub(crate) async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Option<F::Output> {
    poll_fn(|cx| Poll::Ready(ready(future.as_mut().poll(cx)))).await
}

/// Waits for both futures, and gives both outputs.
pub(crate) async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut left, mut right) = (None, None);
    poll_fn(|cx| {
        if left.is_none() {
            left = ready(a.as_mut().poll(cx));
        }
        if right.is_none() {
            right = ready(b.as_mut().poll(cx));
        }
        match (left.take(), right.take()) {
            (Some(l), Some(r)) => Poll::Ready((l, r)),
            (l, r) => {
                (left, right) = (l, r);
                Poll::Pending
            }
        }
    })
    .await
}

fn ready<T>(poll: Poll<T>) -> Option<T> {
    match poll {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}
