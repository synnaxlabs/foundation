//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod handoff;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the publish task waits on hub writer sessions")
)]
mod status;
mod stop;
#[cfg(test)]
#[cfg(not(loom))]
mod tests;

use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use env::thread::Handle;
use types::frame::key_set::Interner;
use types::time::Span;

use crate::handoff::{Give, Take};
use crate::stop::{Guard, Stop};

/// The seams a node runs on. `node`'s entry point builds the real ones from `os`;
/// tests and `acceptance` pass simulated ones from `sim`.
pub struct Config<M> {
    /// Where shards run. One shard starts per core.
    pub shards: env::shards::Shards,
    /// The monotonic clock. Mesh time runs on it.
    pub clock: env::clock::Clock,
    /// The OS clock, a source of mesh time.
    pub wall: env::wall::Wall,
    /// The most bytes the node's pools may commit, split evenly across its shards.
    /// Each shard's part must hold the largest block its buffer reads, else
    /// [`Node::join`] gives [`Error::Buffer`].
    pub budget: usize,
    /// Reserves `len` bytes of address space for one shard's pool. `node` calls it
    /// once for each shard, in order of core.
    pub memory: Box<dyn FnMut(usize) -> Result<M, os::memory::Error>>,
    /// Makes the files of the node's data directory. Each shard calls it once on its
    /// own thread, because a `Files` cannot leave the thread that made it. `node`
    /// opens the buffer of shard `i` in directory `shard-<i>` inside them.
    pub files: Arc<dyn Fn() -> env::files::Files + Send + Sync>,
    /// Randomness for the node's shards.
    pub entropy: env::entropy::Entropy,
}

impl<M> fmt::Debug for Config<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("shards", &self.shards)
            .field("clock", &self.clock)
            .field("wall", &self.wall)
            .field("budget", &self.budget)
            .field("entropy", &self.entropy)
            .finish_non_exhaustive()
    }
}

/// A running node. Call [`Node::stop`] to end it, then [`Node::join`].
#[derive(Debug)]
pub struct Node {
    stop: Stop,
    shards: Vec<Shard>,
    failed: Option<Error>,
    /// The node's interner, once every shard has opened its buffer.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "hub sessions take the interner (#340)")
    )]
    interner: Take<Interner>,
}

/// A started shard, with its error once it fails.
#[derive(Debug)]
struct Shard {
    handle: Handle,
    failed: Arc<OnceLock<Error>>,
}

/// The size of each shard's write-ahead ring, until the disk budget sets it (#342).
const AREA: u64 = 64 << 20;
/// The largest record body of each shard's ring: one group commit.
const BODY_MAX: usize = 1 << 20;
/// The longest an entry waits for its group commit to start.
const COMMIT: Span = Span::from_nanos(2_000_000);

impl Node {
    /// Starts one shard per core, named `shard-<i>`. Each is pinned to core `i` when
    /// the host can pin ([`env::shards::Shards::pinnable`]); else the OS places it.
    /// Each shard owns a `block::Pool` with an even part of the budget; shard 0 also
    /// takes the remainder. Each shard opens its buffer in directory `shard-<i>` of
    /// its files, and makes it there when it is not there. The shards open their
    /// buffers one after another, in order of core. Returns once each shard
    /// runs or one has failed to start. A failed start, a shard with no memory, or a
    /// buffer that does not open stops the node, and [`Node::join`] returns its error.
    ///
    /// # Panics
    ///
    /// If a shard's part of the budget needs more address space than a `usize` holds.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start<M: block::Memory + 'static>(config: Config<M>) -> Self {
        let Config {
            shards,
            clock: monotonic,
            wall,
            budget,
            mut memory,
            files,
            entropy,
        } = config;
        let (mesh, _reader) = clock::Clock::new(monotonic.clone());
        let mut mesh = Some((mesh, wall));
        let stop = Stop::default();
        let (give, mut interner) = handoff::pair();
        give.give(Interner::new());
        let mut started = Vec::new();
        let mut error = None;
        let pinnable = shards.pinnable();
        let cores = shards.cores().get();
        for core in 0..cores {
            // A shard that does not start drops `give`, so `interner` gives `None`.
            let (give, take) = handoff::pair();
            let take = std::mem::replace(&mut interner, take);
            let budget = budget / cores + if core == 0 { budget % cores } else { 0 };
            let config = block::Config { budget };
            let pool = match memory(config.reservation()) {
                Ok(m) => block::Pool::new(config, m),
                Err(e) => {
                    error = Some(Error::Memory { core, error: e });
                    stop.set();
                    break;
                }
            };
            let shard = env::shards::Config {
                name: format!("shard-{core}"),
                core: pinnable.then_some(core),
            };
            // Only the first shard gets the mesh clock.
            let mesh = mesh.take();
            let guard = stop.guard();
            let failed = Arc::new(OnceLock::new());
            let open = Open {
                core,
                take,
                give,
                files: Arc::clone(&files),
                clock: monotonic.clone(),
                entropy: entropy.clone(),
                failed: Arc::clone(&failed),
            };
            let main = move |tasks: env::tasks::Tasks| {
                if let Some((mesh, wall)) = mesh {
                    tasks.spawn(async { mesh.run(wall).await });
                }
                open.serve(Rc::new(pool), tasks, guard)
            };
            match shards.start(shard, main) {
                Ok(handle) => started.push(Shard { handle, failed }),
                Err(e) => {
                    // The driver dropped `main` and its guard, which stopped the node.
                    error = Some(Error::Start(e));
                    break;
                }
            }
        }
        Self {
            stop,
            shards: started,
            failed: error,
            interner,
        }
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
    /// or [`Error::Memory`] for a shard with no memory, else [`Error::Buffer`] for
    /// the first shard by core whose buffer did not open, else [`Error::Panicked`]
    /// for the first shard by core that panicked. Any failed shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let mut first = self.failed;
        let mut panicked = None;
        for shard in self.shards {
            if let Err(e) = shard.handle.join() {
                panicked.get_or_insert(Error::Panicked(e));
            }
            if let Some(error) = shard.failed.get() {
                first.get_or_insert_with(|| error.clone());
            }
        }
        first.or(panicked).map_or(Ok(()), Err)
    }
}

/// The open of a shard's buffer, made before the shard starts. The shards open one
/// after another, in order of core, because each open assigns slots in the node's
/// one interner.
struct Open {
    core: usize,
    take: Take<Interner>,
    give: Give<Interner>,
    files: Arc<dyn Fn() -> env::files::Files + Send + Sync>,
    clock: env::clock::Clock,
    entropy: env::entropy::Entropy,
    failed: Arc<OnceLock<Error>>,
}

impl Open {
    /// Opens the shard's buffer and keeps it until `guard` completes. A failed open
    /// drops `guard`, which stops the node.
    async fn serve(
        self,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
        guard: Guard,
    ) {
        if let Some(buffer) = self.run(pool, tasks).await {
            guard.await;
            drop(buffer);
        }
    }

    /// Waits for the interner, opens the shard's buffer on the shard's thread, and
    /// gives the interner to the next shard. A failed open is kept for
    /// [`Node::join`], keeps the interner from the shards after it, and gives `None`.
    async fn run(
        self,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
    ) -> Option<buffer::Buffer> {
        let mut interner = self.take.await?;
        let config = buffer::Config {
            files: (self.files)(),
            dir: PathBuf::from(format!("shard-{}", self.core)),
            pool,
            clock: self.clock,
            tasks,
            entropy: self.entropy,
            layout: buffer::Layout::new(AREA, BODY_MAX)
                .expect("invariant: the ring sizes of node make a ring"),
            commit: COMMIT,
        };
        match buffer::Buffer::open(config, interner.slots()).await {
            Ok(buffer) => {
                self.give.give(interner);
                Some(buffer)
            }
            Err(error) => {
                let error = Error::Buffer {
                    core: self.core,
                    error,
                };
                self.failed
                    .set(error)
                    .expect("invariant: a shard opens its buffer once");
                None
            }
        }
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
    /// The buffer of the shard on `core` did not open.
    Buffer {
        /// The core of the shard.
        core: usize,
        /// Why it did not open.
        error: buffer::Error,
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
            Self::Buffer { core, error } => {
                write!(f, "cannot open the buffer of shard-{core}: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}
