//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
mod directory;
mod handoff;
mod identity;
mod route;
mod scope;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the publish task waits on hub writer sessions")
)]
mod status;
mod stop;
mod task;
#[cfg(test)]
#[cfg(not(loom))]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::future::poll_fn;
use std::iter;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::Poll;

use document::diagnostic::Diagnostic;
use document::{Document, Source};
use env::thread::Handle;
use types::ed25519::PrivateKey;
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
    /// `shards-<n>` inside the files, opens the buffer of shard `i` in directory
    /// `shard-<i>`, and, with a region, opens the mesh in directory `mesh`.
    pub files: Box<dyn FnMut() -> Box<dyn FnOnce() -> env::files::Files + Send>>,
    /// Randomness for the node's shards.
    pub entropy: env::entropy::Entropy,
    /// The disk budget of the node's rings, split evenly across its shards; shard 0
    /// also takes the remainder. Each ring that the start makes fits its part. A ring
    /// with a checkpoint keeps its size, which can be more than its part.
    pub disk: types::byte::Size,
    /// The network. The node's port binds on it.
    pub net: env::net::Net,
    /// Where the node's one port binds: UDP, and TCP on the same port number once
    /// the port carries TCP (#77).
    pub listen: SocketAddr,
    /// The region whose mesh the node opens, or `None` for no mesh. One founding member
    /// has the node's key, and its card holds the public half of the node's private
    /// key, both from the file `node.key` in the data directory. Only `node` reads that
    /// file, so until the node founds its region itself (#1744), only the tests of
    /// `node`, and the `acceptance` lab, which writes the file first with
    /// [`create_key`], give `Some`. Give the same value at each start: the node keeps
    /// no copy of it, and until the mesh stores it (#1209), a log opened with another
    /// value checks proofs against the wrong voters and starts at another spec. A patch
    /// until the node keeps its region in its data directory when it founds or joins
    /// one, and reads it at each start (#1744). The hub of each task knows each channel
    /// of the founding's `definitions`. Give only a founding that `spec::region::check`
    /// accepts. One with a data channel whose index is not an index of it, or with two
    /// channels of one key, makes shard 0 panic, and [`Node::join`] gives
    /// [`Error::Panicked`].
    pub region: Option<mesh::region::Founding>,
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
            .field("listen", &self.listen)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// A running node. Call [`Node::stop`] to end it, then [`Node::join`].
#[derive(Debug)]
pub struct Node {
    stop: Stop,
    shards: Vec<Shard>,
    failed: Option<Error>,
    /// The tasks for shard 0's hub.
    queue: task::Queue<task::Task>,
    /// The node has a region, so shard 0 has an `ops::Node`.
    regional: bool,
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
/// The transport's flow window of each stream and session, a patch until it is a
/// setting, as [`LIMITS`] is.
const WINDOW: usize = 1 << 20;
/// The most streams of each kind a peer may open, a patch as [`WINDOW`] is.
const STREAMS: NonZeroU32 = NonZeroU32::new(64).expect("not zero");
/// How long a silent peer keeps its session, a patch as [`WINDOW`] is.
const IDLE: Span = Span::from_nanos(30_000_000_000);
/// The largest message of a stream, when the pool holds it, a patch as [`WINDOW`]
/// is.
const MESSAGE: NonZeroUsize = NonZeroUsize::new(1 << 16).expect("not zero");

impl Node {
    /// Binds the node's port at [`Config::listen`], then starts one shard per core,
    /// named `shard-<i>`. A port that does not bind starts no shard, and [`Node::join`]
    /// gives [`Error::Port`]. Each shard is pinned to core `i` when the host can pin
    /// ([`env::shards::Shards::pinnable`]); else the OS places it. Each shard owns a
    /// `block::Pool` with an even part of the budget, and a ring with an even part of
    /// the disk budget; shard 0 also takes each remainder. Unless the node stops first,
    /// shard 0 locks the data directory with the file `lock`, which it holds until each
    /// shard has closed its ring and each task of the mesh has ended, then records the
    /// shard count in the data directory, or checks the one there, and each shard opens
    /// its buffer in directory `shard-<i>` of its files, and makes it there when it is
    /// not there. The shards open their buffers one after another, in order of core.
    /// Once each buffer has opened, shard 0 reads the node's key and private key from
    /// the file `node.key` in the data directory, and makes the file at the first start
    /// once it has mesh time, unless [`create_key`] made it, then opens the mesh of
    /// [`Config::region`] when it has one, then serves the port and admits every peer
    /// that proves its key, until its transport or the mesh's group stops, which stops
    /// the node. Returns once each shard runs or one has failed to start. When the disk
    /// budget holds no ring on each shard, no shard starts, and [`Node::join`] gives
    /// [`Error::Disk`] with the budget, the shard count, and the least budget. A failed
    /// start, a shard with no memory, a data directory that another node holds or that
    /// was made for another shard count, a key file that is not valid, or a buffer or a
    /// mesh that does not open stops the node, and [`Node::join`] returns its error.
    ///
    /// # Panics
    ///
    /// If a shard's part of the budget needs more address space than a `usize` holds,
    /// or if the disk budget holds a ring on each of more than `u32::MAX` cores.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start<M: block::Memory + 'static>(config: Config<M>) -> Self {
        let cores = config.shards.cores().get();
        let regional = config.region.is_some();
        let parts = match parts(config.budget, config.disk, cores) {
            Ok(parts) => parts,
            Err(small) => {
                let count =
                    u64::try_from(cores).expect("invariant: a core count fits a u64");
                let min =
                    types::byte::Size::from_bytes(small.min.saturating_mul(count));
                let error = Error::Disk {
                    disk: config.disk,
                    cores,
                    min,
                };
                return Self::failed(error, regional);
            }
        };
        let Ok(count) = u32::try_from(cores) else {
            panic!("the host has {cores} cores, more than a node numbers");
        };
        let part = match transport::Port::bind(&config.net, config.listen) {
            Ok(bound) => bound.split(NonZeroUsize::MIN).pop(),
            Err(error) => {
                let listen = config.listen;
                return Self::failed(Error::Port { listen, error }, regional);
            }
        };
        let endpoint = Endpoint {
            part: part.expect("invariant: a port splits into the parts asked for"),
            region: config.region.clone(),
            clock: config.clock.clone(),
            entropy: config.entropy.clone(),
        };
        Self::launch(config, endpoint, parts.into_iter().zip(0..count))
    }

    /// A node that failed with `error` before any shard started. `regional` is whether
    /// its config has a region.
    fn failed(error: Error, regional: bool) -> Self {
        Self {
            stop: Stop::default(),
            shards: Vec::new(),
            failed: Some(error),
            queue: task::pair().0,
            regional,
        }
    }

    /// Starts the shards of `config`, each with its part and its number in `parts`.
    /// Shard 0 opens `endpoint`.
    fn launch<M: block::Memory + 'static>(
        config: Config<M>,
        endpoint: Endpoint,
        parts: impl Iterator<Item = ((block::Config, buffer::Layout), u32)>,
    ) -> Self {
        let Config {
            shards,
            clock: monotonic,
            wall,
            mut memory,
            mut files,
            entropy,
            ..
        } = config;
        let stop = Stop::default();
        let cores = shards.cores().get();
        let handoff::Chain { first, last, links } = handoff::chain(cores);
        let (queue, inbox) = task::pair();
        let regional = endpoint.region.is_some();
        let (mesh, clock) = clock::Clock::new(monotonic.clone());
        let serve = Serve {
            interner: last,
            inbox,
            endpoint,
            time: clock.clone(),
        };
        let roles = Role::all(mesh, wall, first, serve, cores);
        let mut started = Vec::new();
        let mut error = None;
        let pinnable = shards.pinnable();
        for (core, ((((config, layout), number), role), (take, give))) in
            parts.zip(roles).zip(links).enumerate()
        {
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
            let make = files();
            let main = move |tasks| open.main(role, make(), pool, tasks, guard);
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
            queue,
            regional,
        }
    }

    /// Calls `task` with the node's hub on shard 0, once each shard has opened its
    /// buffer and, with a region, the mesh has opened, and after each task given
    /// before it, then runs its future. So the code in its closure body runs in the
    /// order of the calls; the futures that tasks give run in no set order. Does not
    /// wait. A node that stops or fails before shard 0 calls a task drops it uncalled.
    /// A task runs on shard 0's thread, so it may hold values that are not `Send`,
    /// such as sessions; it sends its result back through a value it owns. Its future
    /// runs until it completes or shard 0 ends, which drops it. A panic in a task ends
    /// shard 0 and fails the node: [`Node::join`] gives [`Error::Panicked`], unless
    /// the node saw the transport or the mesh's group stop first, which gives
    /// [`Error::Transport`] or [`Error::Group`].
    pub fn spawn<F>(&self, task: impl FnOnce(hub::Hub) -> F + Send + 'static)
    where
        F: Future<Output = ()> + 'static,
    {
        self.queue
            .push(Box::new(move |handles| Box::pin(task(handles.hub.clone()))));
    }

    /// Calls `task` with the operations on the node's mesh on shard 0, in the order
    /// and with the guarantees of [`Node::spawn`]. Each new channel that an apply
    /// makes gets a UUIDv7 key at the midpoint of mesh time.
    ///
    /// # Panics
    ///
    /// When [`Config::region`] is `None`: the node has no mesh.
    pub fn operate<F>(&self, task: impl FnOnce(Rc<ops::Node>) -> F + Send + 'static)
    where
        F: Future<Output = ()> + 'static,
    {
        assert!(self.regional, "`operate` on a node with no region");
        self.queue.push(Box::new(move |handles| {
            let ops = handles
                .ops
                .as_ref()
                .expect("invariant: a node with a region opens its mesh");
            Box::pin(task(Rc::clone(ops)))
        }));
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
    /// The first failure: [`Error::Disk`] for a disk budget that holds no ring on each
    /// shard, [`Error::Port`] for a port that did not bind, [`Error::Start`] for a
    /// shard that could not start or pin, or [`Error::Memory`] for a shard with no
    /// memory, else [`Error::Shards`] or [`Error::Directory`] for a data directory that
    /// shard 0 could not claim, else [`Error::Buffer`] for the first shard by core
    /// whose buffer did not open, [`Error::Key`] or [`Error::Directory`] for a key
    /// file that shard 0 could not read or write, [`Error::Blob`] for a chunk store or
    /// [`Error::Mesh`] for a mesh that did not open, or [`Error::Transport`] or
    /// [`Error::Group`], whichever the node sees stop first, else [`Error::Panicked`]
    /// for the first shard by core that panicked. Any failed shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let shards = self.shards.into_iter().map(|shard| {
            // The shard sets `failed` on its own thread, so read it after the join.
            let joined = shard.handle.join();
            (joined, shard.failed.get().cloned())
        });
        error(self.failed, shards.collect())
    }
}

/// Makes the file `node.key` in `files`, the data directory of a node that has not
/// started, with `key` and `private_key`, and makes it durable. Each start of the node
/// then uses them. For tests and tools that must know a node's key before its first
/// start; a node that starts with no file makes its own key.
///
/// # Errors
///
/// [`Error::Directory`] with [`env::files::Error::Exists`] when the file is there and
/// holds a key, and [`Error::Directory`] for a file call that fails. It writes nothing
/// over a key.
pub async fn create_key(
    files: &env::files::Files,
    key: types::node::Key,
    private_key: PrivateKey,
) -> Result<(), Error> {
    identity::store(files, &identity::Identity { key, private_key }).await
}

/// The error of [`Node::join`]: `failed`, else the first shard error by core, else
/// the first panic by core. `shards` gives each shard's join and error in order of
/// core.
fn error(
    failed: Option<Error>,
    shards: Vec<(Result<(), env::thread::Panicked>, Option<Error>)>,
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

/// A shard's steps, made before the shard starts: shard 0's claim of the data
/// directory, then the open of the shard's buffer, with the shard's home over it, held
/// until the node stops. The shards open one after another, in order of core, because
/// each open assigns slots in the node's one interner.
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

/// Shard 0's own steps: it runs the mesh clock, gives the first interner once it has
/// claimed the data directory for the node's shards, and serves the node's tasks.
struct First {
    mesh: clock::Clock,
    wall: env::wall::Wall,
    give: Give<Interner>,
    serve: Serve,
    /// One end for each other shard, which ends once that shard has closed its ring.
    closed: Vec<Take<()>>,
}

/// The part a shard plays in the node's start and stop.
enum Role {
    /// Shard 0's: it runs the mesh clock, claims the data directory, and holds the
    /// lock until each other shard's ring has closed.
    First(Box<First>),
    /// Each other shard's: the end it drops once its ring has closed.
    Next(Give<()>),
}

impl Role {
    /// The role of each of `cores` shards, in order of core.
    fn all(
        mesh: clock::Clock,
        wall: env::wall::Wall,
        give: Give<Interner>,
        serve: Serve,
        cores: usize,
    ) -> impl Iterator<Item = Self> {
        let (ends, closed): (Vec<_>, Vec<_>) =
            (1..cores).map(|_| handoff::pair()).unzip();
        let first = First {
            mesh,
            wall,
            give,
            serve,
            closed,
        };
        iter::once(Self::First(Box::new(first))).chain(ends.into_iter().map(Self::Next))
    }
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
    /// Runs the shard to its end in its `role`. Shard 0 holds the lock until each
    /// shard's ring has closed, so a node that takes the lock finds no ring open.
    async fn main(
        self,
        role: Role,
        files: env::files::Files,
        pool: block::Pool,
        tasks: env::tasks::Tasks,
        guard: Guard,
    ) {
        match role {
            Role::First(first) => {
                let First {
                    mesh,
                    wall,
                    give,
                    serve,
                    closed,
                } = *first;
                tasks.spawn(async { mesh.run(wall).await });
                let lock = self.claim(&files, closed.len() + 1, give).await;
                let (shard, pool) = (tasks.clone(), Rc::new(pool));
                let (own, mesh_files) = (Rc::clone(&pool), files.clone());
                let failed = Arc::clone(&self.failed);
                let hold = async move |home, guard| {
                    serve
                        .run(home, mesh_files, own, shard, guard, &failed)
                        .await;
                };
                self.keep(files, pool, tasks, guard, hold).await;
                for shard in closed {
                    shard.await;
                }
                drop(lock);
            }
            Role::Next(ended) => {
                let hold = async |home, guard: Guard| {
                    guard.await;
                    drop(home);
                };
                self.keep(files, Rc::new(pool), tasks, guard, hold).await;
                drop(ended);
            }
        }
    }

    /// Opens the shard's buffer and gives its home and `guard` to `hold`, which drops
    /// the home once `guard` completes, then returns once its ring has closed. A
    /// failed open drops `guard`, which stops the node.
    async fn keep(
        self,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
        guard: Guard,
        hold: impl AsyncFnOnce(home::Shard, Guard),
    ) {
        let Some(home) = self.run(files, pool, tasks.clone()).await else {
            // Stops the node, so each other shard ends.
            drop(guard);
            return;
        };
        // Resolves once the home has dropped and the buffer's task has written what
        // was queued and ended, which closes the ring.
        let commit = home.committed();
        hold(home, guard).await;
        // Its error reaches no caller (#1329).
        drop(commit.await);
    }

    /// Claims the data directory for `cores` shards, gives the node's first interner,
    /// and gives the lock of the data directory. A failed claim goes into `failed`
    /// and gives no interner, so no ring opens. So does a stop raised before the
    /// claim, but it is not a failure.
    async fn claim(
        &self,
        files: &env::files::Files,
        cores: usize,
        give: Give<Interner>,
    ) -> Option<env::files::File> {
        if self.stop.raised() {
            return None;
        }
        match directory::claim(files, cores).await {
            Ok(lock) => {
                give.give(Interner::new());
                Some(lock)
            }
            Err(error) => {
                self.failed
                    .set(error)
                    .expect("invariant: shard 0 opens no ring after a failed claim");
                None
            }
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

/// What shard 0 serves the node's tasks and port with: the interner, once the last
/// shard has opened its buffer, the tasks given to the node, its endpoint, and the
/// node's mesh time.
struct Serve {
    interner: Take<Interner>,
    inbox: task::Inbox<task::Task>,
    endpoint: Endpoint,
    time: clock::Reader,
}

/// What shard 0 opens the node's transport and mesh from, but its files, pool, and
/// tasks.
struct Endpoint {
    /// The node's part of its port.
    part: transport::port::Part,
    region: Option<mesh::region::Founding>,
    clock: env::clock::Clock,
    entropy: env::entropy::Entropy,
}

impl Endpoint {
    /// Opens the node's transport on `pool` and `tasks` with `identity`, then, when the
    /// node has a region, the chunk store in directory [`directory::blob`] of `files`,
    /// and the mesh of that region over both, in directory [`directory::mesh`]. Gives
    /// the error of a store or a mesh that did not open.
    async fn open(
        self,
        identity: identity::Identity,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
    ) -> Result<(Rc<transport::Transport>, Option<mesh::Mesh>), Error> {
        let config = transport::Config {
            private_key: identity.private_key.clone(),
            message_bytes_max: MESSAGE,
            window_bytes: WINDOW,
            streams_max: STREAMS,
            idle: IDLE,
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: tasks.clone(),
            pool: Rc::clone(&pool),
        };
        let transport = transport::Transport::new(config, self.part)
            .expect("invariant: a buffer's pool holds a block of a whole UDP payload");
        let transport = Rc::new(transport);
        let Some(region) = self.region else {
            return Ok((transport, None));
        };
        let store = blob::Store::open(blob::Config {
            files: files.clone(),
            dir: directory::blob(),
            pool: Rc::clone(&pool),
        })
        .await
        .map_err(Error::Blob)?;
        let config = mesh::Config {
            key: identity.key,
            private_key: identity.private_key,
            founding: region,
            files,
            dir: directory::mesh(),
            clock: self.clock,
            entropy: self.entropy,
            tasks,
            pool,
            transport: Rc::clone(&transport),
            store: Rc::new(store),
        };
        let mesh = mesh::Mesh::open(config).await.map_err(Error::Mesh)?;
        Ok((transport, Some(mesh)))
    }
}

impl Serve {
    /// Loads the node's identity ([`identity::load`]) and opens the endpoint, then
    /// runs each task given with a hub over `home` that knows each channel of the
    /// region's founding spec, and serves the node's port, until `guard` completes,
    /// the transport stops, or the mesh's group stops, by the rank of [`end`]. A
    /// transport or a group that ends it goes into `failed` before it drops the tasks
    /// given that still run. Before it returns, it drops the tasks, the hub, `home`,
    /// `guard`, each session and stream future, and the mesh, and, with a mesh, waits
    /// for each task of the mesh to end. The transport drops with the last of the
    /// port's future and the tasks of the mesh. Runs no task and takes no session when
    /// a shard did not open, or when the identity did not load or the mesh did not
    /// open, which goes into `failed`.
    async fn run(
        self,
        home: home::Shard,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
        guard: Guard,
        failed: &OnceLock<Error>,
    ) {
        let Some(interner) = self.interner.await else {
            return;
        };
        let fail = |error| {
            failed.set(error).expect(
                "invariant: shard 0 serves only once its claim and open succeed",
            );
        };
        let identity =
            match identity::load(&files, &self.time, &self.endpoint.entropy).await {
                Ok(identity) => identity,
                Err(error) => return fail(error),
            };
        let (key, entropy) = (identity.key, self.endpoint.entropy.clone());
        // The endpoint's open takes the founding, so the definitions go first.
        let definitions = self
            .endpoint
            .region
            .as_ref()
            .map(|region| region.definitions.clone());
        let (transport, mesh) = match self
            .endpoint
            .open(identity, files, pool, tasks.clone())
            .await
        {
            Ok(opened) => opened,
            Err(error) => return fail(error),
        };
        let ops = mesh.as_ref().map(|mesh| {
            Rc::new(operations(mesh.clone(), self.time.clone(), entropy.clone()))
        });
        let hub = hub::Hub::new(hub::Config {
            home,
            interner,
            tasks: tasks.clone(),
            node: key,
            time: self.time,
            entropy,
            mesh: mesh.clone(),
        });
        hub.define(definitions.iter().flatten());
        let ended = mesh.as_ref().map(mesh::Mesh::ended);
        let group = stopped(mesh.as_ref());
        // The port's future holds the mesh, so it drops before the wait.
        {
            let port = route::accept(transport, mesh, hub.clone(), tasks.clone());
            let mut port = pin!(port);
            let mut group = pin!(group);
            let mut guard = pin!(guard);
            let stop = poll_fn(|cx| {
                let ended = end(
                    guard.as_mut().poll(cx),
                    port.as_mut().poll(cx),
                    group.as_mut().poll(cx),
                );
                // Set before the tasks drop, so that `join` ranks it above a panic in
                // a task's drop.
                ended.map(|error| error.map_or((), fail))
            });
            self.inbox
                .serve(task::Handles { hub, ops }, tasks, stop)
                .await;
        }
        if let Some(ended) = ended {
            ended.await;
        }
    }
}

/// A new channel key: a UUIDv7 at the midpoint of mesh time read through `time`,
/// with random bits from `entropy`.
fn channel_key(
    time: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> types::channel::Key {
    // The time only orders keys, so a key before mesh time or 1970 has the time 0.
    // The midpoint, not an edge: only it never goes back, while mesh time is before
    // 2162.
    let at = time.now().mesh.map_or(Stamp::EPOCH, |mesh| {
        Stamp::from_nanos(mesh.earliest.nanos().midpoint(mesh.latest.nanos()))
            .max(Stamp::EPOCH)
    });
    let mut random = [0; 16];
    entropy.fill(&mut random);
    types::channel::Key::v7(at, u128::from_le_bytes(random))
}

/// The operations on `mesh`, whose keys [`channel_key`] makes.
fn operations(
    mesh: mesh::Mesh,
    time: clock::Reader,
    entropy: env::entropy::Entropy,
) -> ops::Node {
    let key = move || channel_key(&time, &entropy);
    let front_ends = BTreeMap::from([("hcl", ops::FrontEnd { read: hcl })]);
    ops::Node::new(mesh, key, front_ends, connector::kind::Table::new())
}

/// The HCL front end.
fn hcl(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>> {
    config_hcl::read(source, text)
        .map_err(|errors| errors.iter().map(Diagnostic::from).collect())
}

/// Resolves with the stop of the group of `mesh`, or never when the node has no mesh.
fn stopped(mesh: Option<&mesh::Mesh>) -> impl Future<Output = mesh::Stopped> + use<> {
    // `next` gives the stop of the group on a watch of any index.
    let watch = mesh.map(|mesh| mesh.watch(types::channel::Key::from_u128(0)));
    async move {
        let Some(mut watch) = watch else {
            return std::future::pending().await;
        };
        loop {
            if let Err(stopped) = watch.next().await {
                return stopped;
            }
        }
    }
}

/// How shard 0's serve ends, from one poll of each cause, in rank order: a stop of
/// the node (`guard`), which gives `None`, then a transport that stopped (`port`),
/// then a group that stopped. The order of the port and the group is a fixed
/// tie-break, with no contract.
fn end(
    guard: Poll<()>,
    port: Poll<transport::Error>,
    group: Poll<mesh::Stopped>,
) -> Poll<Option<Error>> {
    if guard.is_ready() {
        return Poll::Ready(None);
    }
    if let Poll::Ready(error) = port {
        return Poll::Ready(Some(Error::Transport(error)));
    }
    group.map(|stopped| Some(Error::Group(stopped)))
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
    /// A file call that locks the data directory, reads or records its shard count, or
    /// reads or writes the node's key, failed. [`env::files::Error::Busy`] on `lock` is
    /// another node that runs on the data directory.
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
    /// The transport of the node's port stopped, as when the OS breaks its socket.
    /// The node stops.
    Transport(transport::Error),
    /// The mesh did not open. The node took no session.
    Mesh(mesh::Error),
    /// The group of the node's mesh stopped, as when a write of its log fails. The
    /// node stops.
    Group(mesh::Stopped),
    /// The chunk store did not open. The node took no session.
    Blob(blob::Error),
    /// The file `node.key` in the data directory is not a key that a node wrote:
    /// another length that is not 0, or another tag or checksum. The node took no
    /// session and does not write over the file, because a new key is a new node to its
    /// region.
    Key,
    /// The node's port did not bind. No shard started.
    Port {
        /// The address of the bind.
        listen: SocketAddr,
        /// Why it did not bind.
        error: env::net::Error,
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
            Self::Directory(error) => {
                write!(f, "cannot claim the data directory: {error}")
            }
            Self::Disk { disk, cores, min } => write!(
                f,
                "the disk budget {disk} holds no ring on each of {cores} shards; it \
                 needs at least {min}"
            ),
            Self::Transport(error) => {
                write!(f, "the node's transport stopped: {error}")
            }
            Self::Mesh(error) => write!(f, "the node's mesh did not open: {error}"),
            Self::Group(stopped) => {
                write!(f, "the group of the node's mesh stopped: {stopped}")
            }
            Self::Blob(error) => {
                write!(f, "cannot open the node's chunk store: {error}")
            }
            Self::Key => f.write_str(
                "the file node.key in the data directory is not a node key; restore it \
                 from a backup of this node",
            ),
            Self::Port { listen, error } => {
                write!(f, "cannot bind the node's port at {listen}: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}
