//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod directory;
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
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use env::thread::Handle;
use types::frame::key_set::Interner;
use types::time::{Span, Stamp};

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
    pub budget: types::byte::Size,
    /// Reserves `len` bytes of address space for one shard's pool. `node` calls it in
    /// order of core, once for each shard, until a shard gets no memory or does not
    /// start.
    pub memory: Box<dyn FnMut(usize) -> Result<M, os::memory::Error>>,
    /// Makes the files of one shard. `node` calls it on the thread that calls
    /// [`Node::start`], in order of core, once for each shard that gets its memory,
    /// just before that shard starts. The shard runs the function it gives on its own
    /// thread, because a `Files` cannot leave the thread that made it; a shard that
    /// does not start drops it unrun. `node` records the shard count in directory
    /// `shards-<n>` inside the files, and opens the buffer of shard `i` in directory
    /// `shard-<i>`.
    pub files: Box<dyn FnMut() -> Box<dyn FnOnce() -> env::files::Files + Send>>,
    /// Randomness for the node's shards.
    pub entropy: env::entropy::Entropy,
    /// The disk budget of the node's rings, split evenly across its shards; shard 0
    /// also takes the remainder. Each ring that the start makes fits its part. A ring
    /// already there keeps its size, which can be more than its part.
    pub disk: types::byte::Size,
}

impl<M> fmt::Debug for Config<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("shards", &self.shards)
            .field("clock", &self.clock)
            .field("wall", &self.wall)
            .field("budget", &self.budget)
            .field("entropy", &self.entropy)
            .field("disk", &self.disk)
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

/// The largest record body of each shard's ring: one group commit.
const BODY_MAX: usize = 1 << 20;
/// The longest an entry waits for its group commit to start.
const COMMIT: Span = Span::from_nanos(2_000_000);
/// The stamps each home accepts, a patch until they are settings (#1285).
const LIMITS: home::order::Limits = home::order::Limits {
    earliest: Stamp::from_nanos(946_684_800_000_000_000),
    ahead: Span::from_nanos(10_000_000_000),
};

impl Node {
    /// Starts one shard per core, named `shard-<i>`. Each is pinned to core `i` when
    /// the host can pin ([`env::shards::Shards::pinnable`]); else the OS places it.
    /// Each shard owns a `block::Pool` with an even part of the budget, and a ring
    /// with an even part of the disk budget; shard 0 also takes each remainder.
    /// Unless the node stops first, the start records the shard count in the data
    /// directory, or checks the one there, and each shard opens its buffer in
    /// directory `shard-<i>` of its files, and makes it there when it is not there.
    /// The shards open their buffers one after another, in order of core. Returns
    /// once each shard runs or one has failed to start. When the disk budget holds no
    /// ring on each shard, no shard starts, and [`Node::join`] gives [`Error::Disk`]
    /// with the budget, the shard count, and the least budget. A failed start, a shard
    /// with no memory, a data directory made for another shard count, or a buffer that
    /// does not open stops the node, and [`Node::join`] returns its error.
    ///
    /// # Panics
    ///
    /// If a shard's part of the budget needs more address space than a `usize` holds,
    /// or if the disk budget holds a ring on each of more than `u32::MAX` cores.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start<M: block::Memory + 'static>(config: Config<M>) -> Self {
        let cores = config.shards.cores().get();
        match parts(config.budget, config.disk, cores) {
            Ok(parts) => {
                let Ok(count) = u32::try_from(cores) else {
                    panic!("the host has {cores} cores, more than a node numbers");
                };
                Self::spawn(config, parts.into_iter().zip(0..count))
            }
            Err(small) => {
                let count =
                    u64::try_from(cores).expect("invariant: a core count fits a u64");
                let error = Error::Disk {
                    disk: config.disk,
                    cores,
                    min: types::byte::Size::from_bytes(small.min.saturating_mul(count)),
                };
                Self {
                    stop: Stop::default(),
                    shards: Vec::new(),
                    failed: Some(error),
                    interner: handoff::pair().1,
                }
            }
        }
    }

    /// Starts the shards of `config`, each with its part and its number in `parts`.
    fn spawn<M: block::Memory + 'static>(
        config: Config<M>,
        parts: impl Iterator<Item = ((block::Config, buffer::Layout), u32)>,
    ) -> Self {
        let Config {
            shards,
            clock: monotonic,
            wall,
            budget: _,
            mut memory,
            mut files,
            entropy,
            disk: _,
        } = config;
        let stop = Stop::default();
        let (give, mut interner) = handoff::pair();
        let (mesh, clock) = clock::Clock::new(monotonic.clone());
        // Shard 0 runs the mesh clock, and gives the first interner once it has
        // claimed the data directory for the node's cores.
        let mut first = Some((mesh, wall, give, shards.cores().get()));
        let mut started = Vec::new();
        let mut error = None;
        let pinnable = shards.pinnable();
        for (core, ((config, layout), number)) in parts.enumerate() {
            // A shard that does not start drops `give`, so `interner` gives `None`.
            let (give, take) = handoff::pair();
            let take = std::mem::replace(&mut interner, take);
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
            let guard = stop.guard();
            let failed = Arc::new(OnceLock::new());
            let open = Open {
                shard: number,
                take,
                give,
                monotonic: monotonic.clone(),
                clock: clock.clone(),
                entropy: entropy.clone(),
                layout,
                failed: Arc::clone(&failed),
                stop: stop.clone(),
            };
            let first = first.take();
            let make = files();
            let main = move |tasks: env::tasks::Tasks| {
                let files = make();
                if let Some((mesh, wall, give, cores)) = first {
                    tasks.spawn(async { mesh.run(wall).await });
                    tasks.spawn(open.claim(files.clone(), cores, give));
                }
                open.serve(files, Rc::new(pool), tasks, guard)
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

    /// Asks every shard to end. A shard then starts no claim of the data directory
    /// and no open of its buffer; a step that started runs to its end. A stop is not
    /// a failure. Does not wait; call [`Node::join`].
    pub fn stop(&self) {
        self.stop.set();
    }

    /// Blocks until every shard has ended. Call it on a thread that `env` did not
    /// start. Under `sim`, run the sim to its end first.
    ///
    /// # Errors
    ///
    /// The first failure: [`Error::Disk`] for a disk budget that holds no ring on
    /// each shard, [`Error::Start`] for a shard that could not start or pin, or
    /// [`Error::Memory`] for a shard with no memory, else [`Error::Shards`] or
    /// [`Error::Directory`] for a data directory that shard 0 could not claim, else
    /// [`Error::Buffer`] for the first shard by core whose buffer did not open, else
    /// [`Error::Panicked`] for the first shard by core that panicked. Any failed
    /// shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let shards = self.shards.into_iter().map(|shard| {
            // The shard sets `failed` on its own thread, so read it after the join.
            let joined = shard.handle.join();
            (joined, shard.failed.get().cloned())
        });
        error(self.failed, shards)
    }
}

/// The error of [`Node::join`]: `failed`, else the first shard error by core, else
/// the first panic by core. Takes each item of `shards`, which gives each shard's
/// join and error in order of core.
fn error(
    failed: Option<Error>,
    shards: impl Iterator<Item = (Result<(), env::thread::Panicked>, Option<Error>)>,
) -> Result<(), Error> {
    let mut first = failed;
    let mut panicked = None;
    for (joined, failure) in shards {
        if let Err(e) = joined {
            panicked.get_or_insert(Error::Panicked(e));
        }
        if let Some(failure) = failure {
            first.get_or_insert(failure);
        }
    }
    first.or(panicked).map_or(Ok(()), Err)
}

/// The disk steps of a shard, made before the shard starts: shard 0's claim of the
/// data directory, and the open of the shard's buffer, with the shard's home over
/// it. The shards open one after another, in order of core, because each open
/// assigns slots in the node's one interner.
struct Open {
    /// The shard's number on its node: its core.
    shard: u32,
    take: Take<Interner>,
    give: Give<Interner>,
    monotonic: env::clock::Clock,
    /// The node's clocks, for the shard's home.
    clock: clock::Reader,
    entropy: env::entropy::Entropy,
    layout: buffer::Layout,
    failed: Arc<OnceLock<Error>>,
    stop: Stop,
}

/// The pool of each shard from its part of `budget`, and the layout of its ring from
/// its part of `disk`, in order of core, else the first part that holds no ring.
fn parts(
    budget: types::byte::Size,
    disk: types::byte::Size,
    cores: usize,
) -> Result<Vec<(block::Config, buffer::Layout)>, buffer::Small> {
    (0..cores)
        .map(|core| {
            let budget = part(budget.bytes(), cores, core);
            let Ok(budget) = usize::try_from(budget) else {
                panic!(
                    "pool budget {budget} of shard-{core} is past the address space"
                );
            };
            let layout =
                buffer::Layout::fit(part(disk.bytes(), cores, core), BODY_MAX)?;
            Ok((block::Config { budget }, layout))
        })
        .collect()
}

/// The part of `total` of the shard on `core` of `cores`: an even part, and the
/// remainder for shard 0.
fn part(total: u64, cores: usize, core: usize) -> u64 {
    let count = u64::try_from(cores).expect("invariant: a core count fits a u64");
    total / count + if core == 0 { total % count } else { 0 }
}

impl Open {
    /// Claims the data directory for `cores` shards, then gives the node's first
    /// interner. A failed claim goes into `failed` and gives none, so no ring opens.
    /// So does a stop raised before the claim, but it is not a failure.
    fn claim(
        &self,
        files: env::files::Files,
        cores: usize,
        give: Give<Interner>,
    ) -> impl Future<Output = ()> + 'static {
        let (failed, stop) = (Arc::clone(&self.failed), self.stop.clone());
        async move {
            if stop.raised() {
                return;
            }
            match directory::claim(&files, cores).await {
                Ok(()) => give.give(Interner::new()),
                Err(error) => failed
                    .set(error)
                    .expect("invariant: shard 0 opens no ring after a failed claim"),
            }
        }
    }

    /// Opens the shard's buffer, builds its home over it, and keeps the home until
    /// `guard` completes. A failed open drops `guard`, which stops the node.
    async fn serve(
        self,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
        guard: Guard,
    ) {
        if let Some(home) = self.run(files, pool, tasks).await {
            guard.await;
            drop(home);
        }
    }

    /// Waits for the interner, opens the shard's buffer on the shard's thread, gives
    /// the interner to the next shard, and gives the shard's home over the buffer. A
    /// failed open is kept for [`Node::join`], keeps the interner from the shards
    /// after it, and gives `None`. So does a stop raised before the open, but it is
    /// not a failure.
    async fn run(
        self,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
    ) -> Option<home::Shard> {
        let mut interner = self.take.await?;
        if self.stop.raised() {
            return None;
        }
        let core = usize::try_from(self.shard).expect("invariant: a u32 fits a usize");
        let config = buffer::Config {
            files,
            dir: directory::shard(core),
            pool,
            clock: self.monotonic,
            tasks,
            entropy: self.entropy,
            layout: self.layout,
            commit: COMMIT,
        };
        match buffer::Buffer::open(config, interner.slots()).await {
            Ok(buffer) => {
                self.give.give(interner);
                Some(home::Shard::new(home::Config {
                    shard: self.shard,
                    buffer,
                    clock: self.clock,
                    limits: LIMITS,
                }))
            }
            Err(error) => {
                let error = Error::Buffer { core, error };
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
    /// The data directory holds the buffers of `stored` shards, and this node starts
    /// one shard on each of its `cores` cores.
    Shards {
        /// The shard count the data directory was made for.
        stored: usize,
        /// The shard count of this start.
        cores: usize,
    },
    /// A file call that reads or records the shard count of the data directory
    /// failed.
    Directory(env::files::Error),
    /// The disk budget holds no ring on each of `cores` shards.
    Disk {
        /// The disk budget that was given.
        disk: types::byte::Size,
        /// The count of shards.
        cores: usize,
        /// The least disk budget that holds a ring on each shard, capped at the largest
        /// `Size`.
        min: types::byte::Size,
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
            Self::Shards { stored, cores } => write!(
                f,
                "the data directory holds {stored} shards, but this node starts \
                 {cores}; start it on {stored} cores"
            ),
            Self::Directory(error) => write!(
                f,
                "cannot read or record the shard count of the data directory: {error}"
            ),
            Self::Disk { disk, cores, min } => write!(
                f,
                "the disk budget {disk} holds no ring on each of {cores} shards; it \
                 needs at least {min}"
            ),
        }
    }
}

impl std::error::Error for Error {}
