//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod stop;
#[cfg(test)]
#[cfg(not(loom))]
mod tests;

use std::fmt;
use std::sync::{Arc, OnceLock};

use env::thread::Handle;

use crate::stop::Stop;

/// The seams a node runs on. `node`'s entry point builds the real ones from `os`;
/// tests and `acceptance` pass simulated ones from `sim`.
pub struct Config<M> {
    /// Where shards run. One shard starts per core.
    pub shards: env::shards::Shards,
    /// The most bytes the node's pools may commit, split evenly across its shards.
    pub budget: usize,
    /// Reserves `len` bytes of address space for the pool of the shard on `core`, as
    /// `memory(core, len)`. `node` calls it once for each shard, on that shard's
    /// thread.
    pub memory: Arc<dyn Fn(usize, usize) -> Result<M, os::memory::Error> + Send + Sync>,
}

impl<M> fmt::Debug for Config<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("shards", &self.shards)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

/// A running node. Call [`Node::stop`] to end it, then [`Node::join`].
#[derive(Debug)]
pub struct Node {
    stop: Stop,
    handles: Vec<Handle>,
    failed: Option<env::thread::Error>,
    /// By core: why the shard got no pool.
    unpooled: Vec<Arc<OnceLock<os::memory::Error>>>,
}

impl Node {
    /// Starts one shard per core, named `shard-<i>` and pinned to core `i`. Each
    /// shard owns a `block::Pool` with an even part of the budget; shard 0 also takes
    /// the remainder. Returns once each shard runs or one has failed to start. A
    /// failed start, or a shard with no memory, stops the node, and [`Node::join`]
    /// returns its error.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start<M: block::Memory + 'static>(config: Config<M>) -> Self {
        let Config {
            shards,
            budget,
            memory,
        } = config;
        let stop = Stop::default();
        let mut node = Self {
            stop: stop.clone(),
            handles: Vec::new(),
            failed: None,
            unpooled: Vec::new(),
        };
        let cores = shards.cores().get();
        for core in 0..cores {
            let shard = env::shards::Config {
                name: format!("shard-{core}"),
                core: Some(core),
            };
            let budget = budget / cores + if core == 0 { budget % cores } else { 0 };
            let config = block::Config { budget };
            let memory = Arc::clone(&memory);
            let unpooled = Arc::new(OnceLock::new());
            node.unpooled.push(Arc::clone(&unpooled));
            let guard = stop.guard();
            let main = move |_tasks| {
                let pool = memory(core, config.reservation())
                    .map(|m| block::Pool::new(config, m));
                async move {
                    match pool {
                        Ok(pool) => {
                            guard.await;
                            drop(pool);
                        }
                        Err(e) => {
                            unpooled.get_or_init(|| e);
                            drop(guard);
                        }
                    }
                }
            };
            match shards.start(shard, main) {
                Ok(handle) => node.handles.push(handle),
                Err(e) => {
                    // The driver dropped `main` and its guard, which stopped the node.
                    node.failed = Some(e);
                    break;
                }
            }
        }
        node
    }

    /// Asks every shard to end. Does not wait; call [`Node::join`].
    pub fn stop(&self) {
        self.stop.set();
    }

    /// Blocks until every shard has ended. Call it on a thread that `env` did not
    /// start. Under `sim`, run the sim to its end first.
    ///
    /// # Errors
    ///
    /// The first failure: [`Error::Start`] for a shard that could not start or pin,
    /// else [`Error::Memory`] for the first shard by core with no memory, else
    /// [`Error::Panicked`] for the first shard by core that panicked. Any failed
    /// shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let mut first = self.failed.map(Error::Start);
        let mut panicked = None;
        for handle in self.handles {
            if let Err(e) = handle.join() {
                panicked.get_or_insert(Error::Panicked(e));
            }
        }
        for (core, unpooled) in self.unpooled.iter().enumerate() {
            if let Some(&error) = unpooled.get() {
                first.get_or_insert(Error::Memory { core, error });
            }
        }
        first.or(panicked).map_or(Ok(()), Err)
    }
}

/// Why a node failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A shard could not start or pin.
    Start(env::thread::Error),
    /// A shard panicked.
    Panicked(env::thread::Panicked),
    /// The OS gave no memory for the pool of the shard on `core`.
    Memory {
        /// The core of the shard.
        core: usize,
        /// Why the OS gave none.
        error: os::memory::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(e) => write!(f, "{e}"),
            Self::Panicked(e) => write!(f, "{e}"),
            Self::Memory { core, error } => {
                write!(f, "no memory for the pool of shard-{core}: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}
