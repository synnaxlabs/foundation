//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod stop;
#[cfg(test)]
#[cfg(not(loom))]
mod tests;

use std::fmt;

use env::thread::Handle;

use crate::stop::Stop;

/// The seams a node runs on. `node`'s entry point builds the real ones from `os`;
/// tests and `acceptance` pass simulated ones from `sim`.
#[derive(Debug)]
pub struct Config {
    /// Where shards run. One shard starts per core.
    pub shards: env::shards::Shards,
}

/// A running node. Call [`Node::stop`] to end it, then [`Node::join`].
#[derive(Debug)]
pub struct Node {
    stop: Stop,
    handles: Vec<Handle>,
    failed: Option<env::thread::Error>,
}

impl Node {
    /// Starts one shard per core, named `shard-<i>` and pinned to core `i`. Returns
    /// once each shard runs or one has failed to start. A failed start stops the
    /// node, and [`Node::join`] returns its error.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start(config: Config) -> Self {
        let Config { shards } = config;
        let stop = Stop::default();
        let mut node = Self {
            stop: stop.clone(),
            handles: Vec::new(),
            failed: None,
        };
        for core in 0..shards.cores().get() {
            let shard = env::shards::Config {
                name: format!("shard-{core}"),
                core: Some(core),
            };
            let guard = stop.guard();
            match shards.start(shard, move |_tasks| guard) {
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
    /// else [`Error::Panicked`] for the first shard by core that panicked. Any
    /// failed shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let mut first = self.failed.map(Error::Start);
        for handle in self.handles {
            if let Err(e) = handle.join() {
                first.get_or_insert(Error::Panicked(e));
            }
        }
        first.map_or(Ok(()), Err)
    }
}

/// Why a node failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A shard could not start or pin.
    Start(env::thread::Error),
    /// A shard panicked.
    Panicked(env::thread::Panicked),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(e) => write!(f, "{e}"),
            Self::Panicked(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}
