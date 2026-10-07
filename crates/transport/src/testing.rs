//! A sim shard for a [`Config`], and runs of two nodes with a carrier or transport
//! on each.

use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use env::clock::Clock;
use env::entropy::Entropy;
use env::net::Net;
use env::tasks::Tasks;
use sim::Sim;
use sim::node::Node;
use types::node::PrivateKey;
use types::time::Span;

use crate::{Config, Port, Transport, port, quic};

/// The most streams of each kind a peer may open, in [`Shard::config`].
pub(crate) const STREAMS_MAX: u32 = 16;

/// The UDP port of [`address`].
pub(crate) const PORT: u16 = 4433;

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
            message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(STREAMS_MAX).expect("not zero"),
            idle,
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: self.tasks.clone(),
            pool: Rc::clone(&self.pool),
        }
    }

    /// A block from [`Shard::pool`] that holds `bytes`.
    pub(crate) fn block(&self, bytes: &[u8]) -> Block {
        let mut block = self.pool.alloc(bytes.len()).expect("room");
        block.copy_from_slice(bytes);
        block.freeze()
    }

    /// The bytes of [`Shard::pool`] that blocks hold or keep for the next alloc.
    pub(crate) fn committed(&self) -> usize {
        self.pool.committed()
    }
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
    let own = node.clone();
    let config = env::shards::Config {
        name: "transport".into(),
        core: None,
    };
    let started = node.shards().start(config, move |tasks| async move {
        main(Shard::new(&own, tasks).config(key, IDLE), own).await;
    });
    drop(started.expect("a shard"));
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
        main(quic::Carrier::new(config, part), node).await;
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
