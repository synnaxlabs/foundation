//! One simulated node and its settings.

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroUsize;

use types::time::{Monotonic, Span, Stamp};

use crate::state::lock;
use crate::{drivers, net};

/// One simulated node: the `env` handles that its code gets. Clones refer to the same
/// node.
///
/// ```
/// let mut sim = sim::Sim::new(sim::Config::default());
/// let node = sim.node(sim::node::Config::default());
/// assert_eq!(node.clock().now(), sim::node::Config::default().monotonic);
/// ```
#[derive(Clone)]
pub struct Node(pub(crate) drivers::Node);

impl Node {
    /// The node's monotonic clock. A sleep on it panics outside the node's threads.
    #[must_use]
    pub fn clock(&self) -> env::clock::Clock {
        env::clock::Clock::new(self.0.clone())
    }

    /// The node's wall clock.
    #[must_use]
    pub fn wall(&self) -> env::wall::Wall {
        env::wall::Wall::new(self.0.clone())
    }

    /// The node's random bytes: its own stream from the run's seed.
    #[must_use]
    pub fn entropy(&self) -> env::entropy::Entropy {
        env::entropy::Entropy::new(self.0.clone())
    }

    /// Starts shards on the node, which has [`Config::cores`] cores.
    #[must_use]
    pub fn shards(&self) -> env::shards::Shards {
        env::shards::Shards::new(self.0.clone())
    }

    /// Starts dedicated threads on the node.
    #[must_use]
    pub fn threads(&self) -> env::threads::Threads {
        env::threads::Threads::new(self.0.clone())
    }

    /// The node's network, on its [`Node::addresses`]. UDP only: `connect` and
    /// `listen` panic.
    ///
    /// - A bind or a send from an address that is not the node's gives `Error::Io`
    ///   with code 99 (`EADDRNOTAVAIL`). Port 0 binds the lowest free port from
    ///   49152.
    /// - A send to the other family than the socket's gives `Error::Unreachable`.
    /// - Each socket draws its send and receive batch maxes from 1, 8, and 64.
    /// - A datagram is lost when it is over the link's
    ///   [`mtu`](crate::link::Config::mtu), when nothing is bound at its
    ///   destination, or when it would fill the receive queue past
    ///   `recv_buffer_bytes`. The send buffer never fills.
    /// - A socket half panics when it polls outside the node's threads.
    #[must_use]
    pub fn net(&self) -> env::net::Net {
        env::net::Net::new(self.0.clone())
    }

    /// The node's IPv4 and IPv6 addresses, in that order: node `k`, from 0 in the
    /// order of [`Sim::node`](crate::Sim::node), has `10.0.0.0` and `fd00::`, each
    /// plus `k + 1`.
    ///
    /// # Panics
    ///
    /// For the 16,777,215th node and after: `10.0.0.0/8` has no host for them.
    #[must_use]
    pub fn addresses(&self) -> [IpAddr; 2] {
        net::addresses(self.0.node)
    }

    /// Steps the wall clock by `span`, forward or back, as when NTP or an operator
    /// sets it. The monotonic clock does not move. A step can move the end of true
    /// time (see [`Sim::run_for`](crate::Sim::run_for)).
    ///
    /// # Panics
    ///
    /// When the wall leaves the range of a [`Stamp`].
    pub fn step_wall(&self, span: Span) {
        let stepped = lock(&self.0.shared).step_wall(self.0.node, span);
        let node = self.0.node;
        assert!(
            stepped,
            "step_wall({span}) moves the wall of node {node} out of range"
        );
    }

    /// Runs nothing on the node for `span` of true time while its clocks move, as in
    /// a VM pause; a negative span is zero. Each wake in the pause, from a timer or
    /// from another node, polls its task when the pause ends. A pause that overlaps
    /// another ends at the later end, and one past the end of true time never ends.
    pub fn pause(&self, span: Span) {
        lock(&self.0.shared).pause(self.0.node, span);
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Node").field(&self.0.node).finish()
    }
}

/// Settings for one node. Build it with `..Config::default()`: fields get added.
///
/// ```
/// let config = sim::node::Config {
///     wall: types::time::Stamp::EPOCH,
///     ..sim::node::Config::default()
/// };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The core count that [`env::shards::Shards::cores`] reports.
    pub cores: NonZeroUsize,
    /// The monotonic reading when the node is added.
    pub monotonic: Monotonic,
    /// The wall time when the node is added.
    pub wall: Stamp,
}

impl Default for Config {
    /// Four cores, one hour after boot, at 2026-01-01T00:00:00Z.
    fn default() -> Self {
        Self {
            cores: NonZeroUsize::new(4).expect("four is not zero"),
            monotonic: Monotonic::default() + Span::HOUR,
            wall: Stamp::from_nanos(1_767_225_600 * Span::SECOND.nanos()),
        }
    }
}
