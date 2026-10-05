//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod stop;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use env::thread::Handle;

use crate::stop::Stop;

/// The seams a node runs on: real ones from `os` in production, simulated ones from
/// `sim` in tests.
#[derive(Debug)]
pub struct Config {
    /// Where shards run. One shard starts per core.
    pub shards: env::shards::Shards,
}

/// A running node. Call [`Node::stop`] to end it, then [`Node::join`].
#[derive(Debug)]
pub struct Node {
    stop: Arc<Stop>,
    handles: Vec<Handle>,
    failed: Option<env::thread::Error>,
}

impl Node {
    /// Starts one shard per core, named `shard-<i>` and pinned to core `i`. Returns
    /// once each shard runs or one has failed to start. A failed start stops the
    /// node, and [`Node::join`] returns its error.
    #[must_use = "a dropped Node leaves its shards running"]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the node owns its seams; later fields move into its shards"
    )]
    pub fn start(config: Config) -> Self {
        let stop = Arc::new(Stop::default());
        let mut node = Self {
            stop: Arc::clone(&stop),
            handles: Vec::new(),
            failed: None,
        };
        for core in 0..config.shards.cores().get() {
            let shard = env::shards::Config {
                name: format!("shard-{core}"),
                core: Some(core),
            };
            let ends = Ends(Arc::clone(&stop));
            match config.shards.start(shard, move |_tasks| ends.wait()) {
                Ok(handle) => node.handles.push(handle),
                Err(e) => {
                    stop.set();
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
    /// [`Error::Thread`] with the first failure: a shard that could not start or
    /// pin, else the first shard by core that panicked. Any failed shard stops the
    /// node.
    pub fn join(self) -> Result<(), Error> {
        let mut first = self.failed.map(Error::Thread);
        for handle in self.handles {
            if let Err(e) = handle.join() {
                first.get_or_insert(Error::Thread(e));
            }
        }
        first.map_or(Ok(()), Err)
    }
}

/// Why a node failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A shard failed to start, or panicked.
    Thread(env::thread::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Thread(e) => Some(e),
        }
    }
}

/// A shard's hold on the node: when the shard ends for any reason, its future
/// drops this, and the node stops.
struct Ends(Arc<Stop>);

impl Ends {
    async fn wait(self) {
        self.0.wait().await;
    }
}

impl Drop for Ends {
    fn drop(&mut self) {
        self.0.set();
    }
}
