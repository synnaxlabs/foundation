//! One simulated node and its settings.

use std::fmt;
use std::num::NonZeroUsize;

use types::time::{Monotonic, Span, Stamp};

use crate::drivers;

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

    /// Starts shards on the node. A shard asks for a core below [`Config::cores`]
    /// or gets [`env::threads::Error::Pin`].
    #[must_use]
    pub fn shards(&self) -> env::shards::Shards {
        env::shards::Shards::new(self.0.clone())
    }

    /// Starts dedicated threads on the node.
    #[must_use]
    pub fn threads(&self) -> env::threads::Threads {
        env::threads::Threads::new(self.0.clone())
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Node").field(&self.0.node).finish()
    }
}

/// Settings for one node.
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
    /// The monotonic reading when the run starts.
    pub monotonic: Monotonic,
    /// The wall time when the run starts.
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
