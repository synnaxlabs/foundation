//! A sim shard for a [`Config`].

use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use env::clock::Clock;
use env::entropy::Entropy;
use env::tasks::Tasks;
use types::node::PrivateKey;
use types::time::Span;

use crate::Config;

/// The most streams of each kind a peer may open, in [`Shard::config`].
pub(crate) const STREAMS_MAX: u32 = 16;

/// What one sim shard gives a [`Config`].
pub(crate) struct Shard {
    clock: Clock,
    entropy: Entropy,
    tasks: Tasks,
    /// The pool of each config.
    pool: Rc<Pool>,
}

impl Shard {
    /// What a shard of `node` with `tasks` gives, with a pool of 4 MiB.
    pub(crate) fn new(node: &sim::node::Node, tasks: Tasks) -> Self {
        let config = block::Config { budget: 1 << 22 };
        let memory = Heap::new(config.reservation());
        Self {
            clock: node.clock(),
            entropy: node.entropy(),
            tasks,
            pool: Rc::new(Pool::new(config, memory)),
        }
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
