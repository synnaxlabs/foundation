//! The composition root: real seams, pools and shards, all tables (kinds, front
//! ends, time sources, secret stores), the status collector, process lifecycle, and
//! upgrades.

mod address;
mod admin;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
mod budget;
mod cause;
pub mod client;
mod connectors;
mod directory;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod fuzz;
mod handoff;
mod identity;
mod log;
mod name;
mod root;
mod route;
mod scope;
mod sector;
mod settings;
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
mod trust;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::future::poll_fn;
use std::iter;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::Poll;

use env::thread::Handle;
use types::frame::key_set::Interner;
use types::name::Prefix;
use types::time::{Span, Stamp};

pub use crate::budget::{Budget, budget};
use crate::handoff::{Give, Take};
pub use crate::log::{Flush, Log};
pub use crate::settings::Settings;
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
    /// The node's budgets. Once each shard has opened its buffer, the start writes them
    /// to the file `budget` of the data directory when that file is not there. A file
    /// that is there stays as it is, also when it holds other budgets. Get the kept
    /// ones with [`budget`] before the start.
    pub budget: Budget,
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
    /// The network. The node's port binds on it.
    pub net: env::net::Net,
    /// Where the node's one port binds: UDP, and TCP on the same port number once
    /// the port carries TCP (#77). A multicast IP or the IPv4 broadcast IP starts no
    /// shard ([`Error::Listen`]).
    pub listen: SocketAddr,
    /// The region whose mesh the node opens.
    pub region: Region,
    /// The node's name. The first start on a data directory keeps it in the file
    /// `name`. Get it with [`name`] before the start. A later start with another name
    /// than the stored one fails with [`Error::Renamed`], and a name with a reserved
    /// segment ([`Error::Reserved`]) or of more than 191 bytes ([`Error::Long`]) starts
    /// no shard. Once the data directory stores a founding with the node's card, that
    /// card holds the name too, and a start whose file `name` is gone writes it again
    /// from the card. [`Node::join`] gives the first failure.
    pub name: types::name::Name,
    /// The limits of the node's port, its peers, and its chunk store. A value that the
    /// transport refuses stops the node with [`Error::Transport`].
    pub settings: Settings,
    /// The environment of the node process, which `env()` in a config reads. Main
    /// gives `std::env::vars_os()`.
    pub vars: BTreeMap<OsString, OsString>,
    /// The CA certificates that each `https` connect of a connector trusts. Main gives
    /// [`os::roots::read()`]. When the read has a failure, shard 0 logs a warning with
    /// the count of its failures and its [`os::roots::Source`], cut to 1024 bytes, then
    /// one for each of the first 16 failures, its text cut the same way. It logs one
    /// for the count of certificates that do not parse, and one when no certificate is
    /// left. It logs each of them also with [`Region::Absent`], which runs no
    /// connector. With a region, the node gives the failures to
    /// [`ops::Config::roots`], which `foundation status` shows, and the first warning
    /// names that command.
    pub roots: os::roots::Roots,
    /// Where and how the node writes its log. Each shard writes the events of its
    /// thread there, and [`Node::join`] the event `stopped`.
    pub log: Log,
    /// Starts the thread `log`, which writes each line of the log, so no event waits
    /// on [`Log::writer`]. Its events go nowhere.
    pub threads: env::threads::Threads,
}

impl<M> fmt::Debug for Config<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("shards", &self.shards)
            .field("clock", &self.clock)
            .field("wall", &self.wall)
            .field("budget", &self.budget)
            .field("entropy", &self.entropy)
            .field("listen", &self.listen)
            .field("region", &self.region)
            .field("name", &self.name)
            .field("settings", &self.settings)
            .field("log", &self.log)
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

/// The region whose mesh a node opens. The hub of each task knows each channel of the
/// region's spec in use, at the open and after each spec change that takes effect.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Region {
    /// No mesh: the node serves only its own hub.
    Absent,
    /// The region that the data directory holds. When its mesh directory holds no
    /// founding, the node founds the root region alone: it is the one member and voter,
    /// and the spec holds the first admin, whose private key the node reads from the
    /// file `admin.key`, or makes and writes there. A start that finds a founding opens
    /// that region and reads no `admin.key`.
    Stored,
    /// The region `founding`, a patch until a node joins by ticket (#1157). One
    /// founding member has the node's key, and its card holds the public half of the
    /// node's private key, both from the file `node.key` in the data directory. That
    /// card also holds `Config::name`: it is the stored name when the file `name` is
    /// gone. Only `node` reads `node.key`, so only the tests of `node`, and the
    /// `acceptance` lab, which writes the file first with `create_key`, give it. A
    /// start whose mesh log holds no record keeps the region in the data directory. A
    /// start whose log holds a record and that gives another region stops the node, and
    /// [`Node::join`] gives [`Error::Mesh`] with [`mesh::Error::Founding`]. At a start
    /// whose log holds no record, the spec in use is the founding's `definitions`, or
    /// no channel when `spec::region::check` gives them problems.
    #[cfg(feature = "sim")]
    Given(mesh::region::Founding),
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
    /// Where the node writes its log.
    log: Flush,
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
/// The interval of each shard's purge timer.
const PURGE: Span = Span::from_nanos(10_000_000_000);
/// The stamps each home accepts, a patch until they are settings (#1285).
const LIMITS: home::order::Limits = home::order::Limits {
    earliest: Stamp::from_nanos(946_684_800_000_000_000),
    ahead: Span::from_nanos(10_000_000_000),
};

impl Node {
    /// Starts one shard per core, named `shard-<i>`. Each shard is pinned to core `i`
    /// when the host can pin ([`env::shards::Shards::pinnable`]); else the OS places
    /// it. Each shard owns a `block::Pool` with an even part of the budget, and a ring
    /// with an even part of the disk budget; shard 0 also takes each remainder. A shard
    /// gives the pages of a block size of its pool back to the system 10 to 20 s after
    /// the last block of that size returned, or later when the shard's timer wakes
    /// late. A size with no block in use gives its pages back sooner when an `alloc` of
    /// another size needs its budget. Unless the node stops first, shard 0 locks the
    /// data directory with the file `lock`, which it holds until each shard has closed
    /// its ring, each task of the mesh has ended, and the transport has freed the port,
    /// then records the shard count and [`Config::name`] in the data directory, or
    /// checks the ones there, then binds the node's port at [`Config::listen`], and
    /// each shard opens its buffer in directory `shard-<i>` of its files, and makes it
    /// there when it is not there. The shards open their buffers one after another, in
    /// order of core. Once each buffer has opened, shard 0 reads the node's key,
    /// private key, and seal key from the file `node.key` in the data directory, and
    /// makes the file at the first start once it has mesh time, unless `create_key`
    /// made it, then opens the mesh of [`Config::region`] when it has one (a
    /// [`Region::Stored`] with no founding first founds the root region and reads or
    /// makes `admin.key`), then writes the file `address` in the data directory, with
    /// the node's key, the public key of its transport, and the bound address, then
    /// serves the port until its transport or the mesh's group stops, which stops the
    /// node. It writes `address` empty when it stops serving, before the lock drops. An
    /// address in the file is no proof that a node runs: a crash or a failed empty
    /// write leaves it. It admits each member of the region, and gives other peers at
    /// most [`Settings::places_max`] places at once: one for each session of a program,
    /// and one for the key of a node. At the bound, the oldest place frees for a new
    /// peer, and its session closes unless its peer is now a member, so a peer outside
    /// the region keeps its session until that many later peers took a place. Returns
    /// once each shard runs or one has failed to start. No shard starts when the disk
    /// budget holds no ring on each shard ([`Error::Disk`]), when a shard's part of the
    /// pool budget gives a reservation of more than `usize::MAX` bytes
    /// ([`Error::Pool`]), when [`Config::listen`] has a multicast IP or the IPv4
    /// broadcast IP ([`Error::Listen`]), or when [`Config::name`] has a reserved
    /// segment ([`Error::Reserved`]) or more than 191 bytes ([`Error::Long`]). A port
    /// that does not bind stops the node after shard 0 claimed the data directory, and
    /// no buffer opens ([`Error::Port`]). Any failure stops the node, and
    /// [`Node::join`] gives the first one (its `# Errors`).
    ///
    /// # Panics
    ///
    /// If the disk budget holds a ring on each of more than `u32::MAX` cores.
    #[must_use = "a dropped Node leaves its shards running"]
    pub fn start<M: block::Memory + 'static>(config: Config<M>) -> Self {
        let (mesh, time) = clock::Clock::new(config.clock.clone());
        let regional = !matches!(config.region, Region::Absent);
        match plan(&config) {
            Ok(parts) => Self::launch(config, parts, (mesh, time), regional),
            Err(error) => {
                let log = log::open(config.log, &config.threads, config.wall, time);
                Self::failed(error, log, regional)
            }
        }
    }

    /// A node that failed with `error` before any shard started.
    fn failed(error: Error, log: Flush, regional: bool) -> Self {
        Self {
            stop: Stop::default(),
            shards: Vec::new(),
            failed: Some(error),
            queue: task::pair().0,
            regional,
            log,
        }
    }

    /// Starts the shards of `config`, each with its number and part in `parts`,
    /// with the mesh clock and its reader in `clock`.
    #[expect(
        clippy::too_many_lines,
        reason = "the start of each shard, whose parts each come from the one before"
    )]
    fn launch<M: block::Memory + 'static>(
        config: Config<M>,
        parts: Vec<(u32, block::Config, buffer::Layout)>,
        (mesh, clock): (clock::Clock, clock::Reader),
        regional: bool,
    ) -> Self {
        let bind = Bind::new(&config);
        let Config {
            shards,
            clock: monotonic,
            wall,
            mut memory,
            mut files,
            entropy,
            log,
            threads,
            ..
        } = config;
        let stop = Stop::default();
        let cores = shards.cores().get();
        let handoff::Chain { first, last, links } = handoff::chain(cores);
        let (queue, inbox) = task::pair();
        let log = log::open(log, &threads, wall.clone(), clock.clone());
        let hold = match log.hold() {
            Ok(hold) => Arc::new(hold),
            Err(e) => return Self::failed(Error::Start(e), log, regional),
        };
        let shards = shards.logged(log.dispatch.clone());
        let serve = Serve {
            interner: last,
            budget: config.budget,
            inbox,
            time: clock.clone(),
            vars: Arc::new(config.vars),
            roots: config.roots,
            name: config.name.clone(),
            shards: cores,
        };
        let roles = Role::all(mesh, wall, first, bind, serve, cores);
        let mut started = Vec::new();
        let mut error = None;
        for (core, (((number, config, layout), role), (take, give))) in
            parts.into_iter().zip(roles).zip(links).enumerate()
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
                core: shards.pinnable().then_some(core),
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
            let hold = Arc::clone(&hold);
            let main = move |tasks| {
                let main = open.main(role, make(), pool, tasks, guard);
                async move {
                    main.await;
                    drop(hold);
                }
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
            queue,
            regional,
            log,
        }
    }

    /// Calls `task` with the node's hub on shard 0, once each shard has opened its
    /// buffer, the file `address` holds the node's address, the node takes sessions,
    /// and, with a region, the mesh has opened, and after each task given before it,
    /// then runs its future. So the code in its closure body runs in the order of the
    /// calls; the futures that tasks give run in no set order. Does not wait. A node
    /// that stops or fails before shard 0 calls a task drops it uncalled. A task runs
    /// on shard 0's thread, so it may hold values that are not `Send`, such as
    /// sessions; it sends its result back through a value it owns. Its future runs
    /// until it completes or shard 0 ends, which drops it. A panic in a task ends shard
    /// 0 and fails the node: [`Node::join`] gives [`Error::Panicked`], unless the node
    /// saw the transport or the mesh's group stop first, which gives
    /// [`Error::Transport`] or [`Error::Group`], or did not write the file `address`,
    /// which gives [`Error::Directory`].
    pub fn spawn<F>(&self, task: impl FnOnce(hub::Hub) -> F + Send + 'static)
    where
        F: Future<Output = ()> + 'static,
    {
        self.queue
            .push(Box::new(move |handles| Box::pin(task(handles.hub.clone()))));
    }

    /// Calls `task` with the operations on the node's mesh on shard 0, in the order
    /// and with the guarantees of [`Node::spawn`]. Each new channel that an apply
    /// makes gets a UUIDv7 key at mesh time.
    ///
    /// # Panics
    ///
    /// When [`Config::region`] is [`Region::Absent`]: the node has no mesh.
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

    /// A handle that waits for the log to reach its writer, for the end of a process.
    #[must_use]
    pub fn flush(&self) -> Flush {
        self.log.clone()
    }

    /// A handle that stops this node from another thread.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop.clone())
    }

    /// Blocks until every shard has ended, then writes the event `stopped` to the log:
    /// INFO, or ERROR with the field `error`. Does not wait for the log to write it:
    /// call [`Flush::wait`], or under `sim`, run the sim again. When the thread `log`
    /// cannot start here, the line waits for [`Flush::wait`]. Call it on a thread that
    /// `env` did not start. Under `sim`, run the sim to its end first.
    ///
    /// # Errors
    ///
    /// The first failure: [`Error::Reserved`] for a name with a reserved segment,
    /// [`Error::Long`] for a name of more than 191 bytes, [`Error::Listen`] for a
    /// [`Config::listen`] with a multicast IP or the IPv4 broadcast IP, [`Error::Disk`]
    /// for a disk budget that holds no ring on each shard, [`Error::Pool`] for a part
    /// of the pool budget that needs a reservation of more than `usize::MAX` bytes,
    /// [`Error::Start`] for the thread `log` or a shard that could not start, or a
    /// shard that could not pin, or [`Error::Memory`] for a shard with no memory, else
    /// [`Error::Shards`] or [`Error::Directory`] for a data directory that shard 0
    /// could not claim, else [`Error::NoCard`] for a data directory with the directory
    /// of a mesh and no file `node.key`, or [`Error::Directory`] for a check of them
    /// that fails, else [`Error::Name`], [`Error::Renamed`], [`Error::NoCard`],
    /// [`Error::Mesh`], [`Error::Key`], or [`Error::Directory`] for a stored name that
    /// it could not read or a name it could not keep, else [`Error::Port`] for a port
    /// that did not bind, else [`Error::Buffer`] for the first shard by core whose
    /// buffer did not open, [`Error::Budget`] or [`Error::Directory`] for a file
    /// `budget` that shard 0 could not read or write, [`Error::Key`], [`Error::Admin`],
    /// or [`Error::Directory`] for a key file that shard 0 could not read or write,
    /// [`Error::Transport`] for a value of [`Config::settings`] that the transport
    /// refuses, [`Error::Blob`] for a chunk store or [`Error::Mesh`] for a stored
    /// region or a mesh that did not open, [`Error::Directory`] for a file `address`
    /// that shard 0 could not write, or [`Error::Transport`] or [`Error::Group`],
    /// whichever the node sees stop first, else [`Error::Directory`] for a file
    /// `address` that shard 0 could not write empty, else [`Error::Panicked`] for the
    /// first shard by core that panicked. Any failed shard stops the node.
    pub fn join(self) -> Result<(), Error> {
        let shards = self.shards.into_iter().map(|shard| {
            // The shard sets `failed` on its own thread, so read it after the join.
            let joined = shard.handle.join();
            (joined, shard.failed.get().cloned())
        });
        let joined = error(self.failed, shards.collect());
        tracing::dispatcher::with_default(&self.log.dispatch, || match &joined {
            Ok(()) => tracing::info!("stopped"),
            Err(error) => tracing::error!(%error, "stopped"),
        });
        // A thread that cannot start leaves the line for `Flush::wait`.
        self.log.resume();
        joined
    }
}

/// Stops its node from any thread, as [`Node::stop`] does. Clones stop one node. A
/// stop after the node ended does nothing.
#[derive(Clone, Debug)]
pub struct Stopper(Stop);

impl Stopper {
    /// Asks every shard of the node to end, as [`Node::stop`] does.
    pub fn stop(&self) {
        self.0.set();
    }
}

/// Makes the file `node.key` in `files`, the data directory of a node that has not
/// started, with `key`, `private_key`, and a new seal key from `entropy`, and makes it
/// durable. Each start of the node then uses them. Gives the public seal key, for the
/// node's card. For tests that must know a node's keys before its first start; a node
/// that starts with no file makes its own keys.
///
/// # Errors
///
/// [`Error::Directory`] with [`env::files::Error::Exists`] when a file is there, before
/// it writes anything, and [`Error::Directory`] for a file call that fails.
#[cfg(feature = "sim")]
pub async fn create_key(
    files: &env::files::Files,
    entropy: &env::entropy::Entropy,
    key: types::node::Key,
    private_key: types::ed25519::PrivateKey,
) -> Result<types::node::SealKey, Error> {
    let opener = secret::seal::Opener::generate(entropy);
    let seal_key = opener.public();
    let identity = identity::Identity {
        key,
        private_key,
        opener,
    };
    identity::write_new(files, &identity).await?;
    Ok(seal_key)
}

/// The name of the node of the data directory `files`: `given`, else the one that the
/// file `name` holds, else the one in the node's card in the founding that the data
/// directory stores. Reads only when `given` is `None`, writes nothing, and reads
/// also while another node runs on `files`. It does not check `given`: the start
/// does ([`Config::name`]).
///
/// # Errors
///
/// When `given` is `None`, with no file `name`: [`Error::Unnamed`] when the data
/// directory stores no founding, [`Error::NoCard`] when it stores a founding with no
/// card of the node (no file `node.key`, or no card of its key), [`Error::Key`] for a
/// file `node.key` that a node did not write, and [`Error::Mesh`] for a founding that
/// does not read. [`Error::Name`] for a file `name` that a node did not write, and
/// [`Error::Directory`] for a file call that fails.
pub async fn name(
    files: &env::files::Files,
    given: Option<types::name::Name>,
) -> Result<types::name::Name, Error> {
    match given {
        Some(given) => Ok(given),
        None => name::stored(files).await?.ok_or(Error::Unnamed),
    }
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
/// directory and bind of the port, then the open of the shard's buffer, with the
/// shard's home over it, held until the node stops. The shards open one after
/// another, in order of core, because each open assigns slots in the node's one
/// interner.
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
/// claimed the data directory for the node's shards and bound the port, and serves the
/// node's tasks.
struct First {
    mesh: clock::Clock,
    wall: env::wall::Wall,
    give: Give<Interner>,
    bind: Bind,
    serve: Serve,
    /// One end for each other shard, which ends once that shard has closed its ring.
    closed: Vec<Take<()>>,
}

/// The part a shard plays in the node's start and stop.
enum Role {
    /// Shard 0's: it runs the mesh clock, claims the data directory, binds the port,
    /// and holds the lock until each other shard's ring has closed and the port is
    /// free.
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
        bind: Bind,
        serve: Serve,
        cores: usize,
    ) -> impl Iterator<Item = Self> {
        let (ends, closed): (Vec<_>, Vec<_>) =
            (1..cores).map(|_| handoff::pair()).unzip();
        let first = First {
            mesh,
            wall,
            give,
            bind,
            serve,
            closed,
        };
        iter::once(Self::First(Box::new(first))).chain(ends.into_iter().map(Self::Next))
    }
}

/// Whether `ip`, in its canonical form, is a multicast IP or the IPv4 broadcast IP.
fn unreachable(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_multicast() || ip == Ipv4Addr::BROADCAST
}

/// The plan of the shards of a `config` that passes its checks: the number, the part,
/// and the layout of each shard, in order of core. Else the error that stops the node
/// before any shard starts.
///
/// # Panics
///
/// If the disk budget holds a ring on each of more than `u32::MAX` cores.
fn plan<M>(
    config: &Config<M>,
) -> Result<Vec<(u32, block::Config, buffer::Layout)>, Error> {
    let cores = config.shards.cores().get();
    if config.name.reserved() {
        let name = config.name.clone();
        return Err(Error::Reserved { name });
    }
    if config.name.as_str().len() > name::MAX_BYTES {
        let name = config.name.clone();
        return Err(Error::Long { name });
    }
    if unreachable(config.listen.ip()) {
        let listen = config.listen;
        return Err(Error::Listen { listen });
    }
    let parts =
        parts(config.budget.pool, config.budget.disk, cores).map_err(|small| {
            let count =
                u64::try_from(cores).expect("invariant: a core count fits a u64");
            Error::Disk {
                disk: config.budget.disk,
                cores,
                min: types::byte::Size::from_bytes(small.min.saturating_mul(count)),
            }
        })?;
    let Ok(count) = u32::try_from(cores) else {
        panic!("the host has {cores} cores, more than a node numbers");
    };
    let parts = parts
        .into_iter()
        .zip(0..count)
        .map(|((pool, layout), shard)| Ok((shard, block::Config::new(pool)?, layout)))
        .collect::<Result<Vec<_>, block::Unfit>>();
    let Ok(parts) = parts else {
        let pool = config.budget.pool;
        return Err(Error::Pool { pool, cores });
    };
    Ok(parts)
}

/// The part of `budget` of each shard, and the layout of its ring from its part of
/// `disk`, in order of core, else the first part that holds no ring.
fn parts(
    budget: types::byte::Size,
    disk: types::byte::Size,
    cores: usize,
) -> Result<Vec<(u64, buffer::Layout)>, buffer::Small> {
    (0..cores)
        .map(|core| {
            let layout =
                buffer::Layout::fit(part(disk.bytes(), cores, core), BODY_MAX)?;
            Ok((part(budget.bytes(), cores, core), layout))
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
    /// shard's ring has closed and the transport has freed the port, so a node that
    /// takes the lock finds no ring open and the port free.
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
                    bind,
                    serve,
                    closed,
                } = *first;
                tasks.spawn(async { mesh.run(wall).await });
                let (cores, name) = (serve.shards, &serve.name);
                let claimed = self.claim(&files, cores, name, bind, give).await;
                let Some((lock, endpoint)) = claimed else {
                    return;
                };
                let (shard, pool) = (tasks.clone(), Rc::new(pool));
                let (own, mesh_files) = (Rc::clone(&pool), files.clone());
                let failed = Arc::clone(&self.failed);
                let hold = async move |home, guard| {
                    serve
                        .run(endpoint, home, mesh_files, own, shard, guard, &failed)
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

    /// Starts the pool's purge timer, which ends with the shard, then opens the shard's
    /// buffer and gives its home and `guard` to `hold`, which drops the home once
    /// `guard` completes, then returns once its ring has closed. A failed open drops
    /// `guard`, which stops the node.
    async fn keep(
        self,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        tasks: env::tasks::Tasks,
        guard: Guard,
        hold: impl AsyncFnOnce(home::Shard, Guard),
    ) {
        let (clock, own) = (self.monotonic.clone(), Rc::clone(&pool));
        tasks.spawn(async move {
            loop {
                clock.sleep(PURGE).await;
                own.purge();
            }
        });
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

    /// Claims the data directory for `cores` shards and `name`, then binds the port
    /// with `bind`, gives the node's first interner, and gives the lock of the data
    /// directory and the endpoint. A failed claim or bind goes into `failed` and gives
    /// no interner, so no ring opens. So does a stop raised before the claim, but it
    /// is not a failure.
    async fn claim(
        &self,
        files: &env::files::Files,
        cores: usize,
        name: &types::name::Name,
        bind: Bind,
        give: Give<Interner>,
    ) -> Option<(env::files::File, Endpoint)> {
        if self.stop.raised() {
            return None;
        }
        let claimed = async {
            let lock = directory::claim(files, cores).await?;
            identity::claim(files).await?;
            name::keep(files, name).await?;
            Ok::<_, Error>((lock, bind.bind(name)?))
        };
        match claimed.await {
            Ok(claimed) => {
                give.give(Interner::new());
                Some(claimed)
            }
            Err(error) => {
                self.failed
                    .set(error)
                    .expect("invariant: shard 0 opens no ring after a failed claim");
                None
            }
        }
    }

    /// Waits for the interner, opens the shard's buffer and its home on the shard's
    /// thread, gives the interner to the next shard, and gives the home. A failed
    /// open is kept for [`Node::join`], keeps the interner from the shards after it,
    /// and gives `None`. So does a stop raised before the open, but it is not a
    /// failure.
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
            trimmed: false,
        };
        match buffer::Buffer::open(config, interner.slots()).await {
            Ok(buffer) => {
                let shard = home::Shard::open(home::Config {
                    shard: self.shard,
                    buffer,
                    clock: self.clock,
                    limits: LIMITS,
                });
                self.give.give(interner);
                Some(shard)
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
/// shard has opened its buffer, the tasks given to the node, and the node's mesh time.
/// Also the budget that shard 0 keeps once the interner comes.
struct Serve {
    interner: Take<Interner>,
    budget: Budget,
    inbox: task::Inbox<task::Task>,
    time: clock::Reader,
    /// The environment of the node process, which the node's connectors read.
    vars: Arc<BTreeMap<OsString, OsString>>,
    /// The CA certificates that the node's connectors trust, which the event `started`
    /// follows.
    roots: os::roots::Roots,
    /// The node's name and shard count, which the claim and the event `started` read.
    name: types::name::Name,
    shards: usize,
}

/// What shard 0 binds the node's port with, once it claimed the data directory.
struct Bind {
    listen: SocketAddr,
    region: Region,
    clock: env::clock::Clock,
    entropy: env::entropy::Entropy,
    net: env::net::Net,
    settings: Settings,
}

impl Bind {
    /// What shard 0 binds the port of the node of `config` with.
    fn new<M>(config: &Config<M>) -> Self {
        Self {
            listen: config.listen,
            region: config.region.clone(),
            clock: config.clock.clone(),
            entropy: config.entropy.clone(),
            net: config.net.clone(),
            settings: config.settings,
        }
    }

    /// Binds the port at `listen`, and gives the endpoint of the node of `name` on it.
    ///
    /// # Errors
    ///
    /// [`Error::Port`] when the port does not bind.
    fn bind(self, name: &types::name::Name) -> Result<Endpoint, Error> {
        let listen = self.listen;
        let bound = transport::Port::bind(&self.net, listen)
            .map_err(|error| Error::Port { listen, error })?;
        let region = match self.region {
            Region::Absent => Opening::Absent,
            Region::Stored => Opening::Stored {
                name: name.clone(),
                addresses: root::addresses(bound.addresses()),
            },
            #[cfg(feature = "sim")]
            Region::Given(founding) => Opening::Given(founding),
        };
        Ok(Endpoint::new(
            bound,
            region,
            self.clock,
            self.entropy,
            self.net,
            self.settings,
        ))
    }
}

/// What shard 0 opens the node's transport and mesh from, but its files, pool, and
/// tasks.
struct Endpoint {
    /// The node's part of its port.
    part: transport::port::Part,
    /// Where the port is bound.
    bound: SocketAddr,
    region: Opening,
    clock: env::clock::Clock,
    entropy: env::entropy::Entropy,
    /// The network, which the node's connectors reach their devices and stores on.
    net: env::net::Net,
    settings: Settings,
}

/// The region that shard 0 opens, with what it needs to found the root region.
enum Opening {
    /// No region: the node opens no mesh.
    Absent,
    /// The founding in the mesh directory, else the root region with a card of `name`
    /// and `addresses`.
    Stored {
        name: types::name::Name,
        addresses: mesh::card::addresses::Addresses,
    },
    /// This founding.
    #[cfg(feature = "sim")]
    Given(mesh::region::Founding),
}

impl Opening {
    /// The founding of the region in `files`, or `None` for no region. When the mesh
    /// directory holds no founding, it is the root region that [`root::founding`]
    /// gives.
    ///
    /// # Errors
    ///
    /// [`Error::Mesh`] when the stored founding does not read, and the errors of
    /// [`root::founding`].
    async fn founding(
        self,
        files: &env::files::Files,
        pool: Rc<block::Pool>,
        time: &clock::Reader,
        entropy: &env::entropy::Entropy,
        identity: &identity::Identity,
    ) -> Result<Option<mesh::region::Founding>, Error> {
        match self {
            Self::Absent => Ok(None),
            Self::Stored { name, addresses } => {
                let stored = mesh::founding(files, &directory::mesh(), pool).await;
                if let Some(founding) = stored.map_err(Error::Mesh)? {
                    return Ok(Some(founding));
                }
                let root =
                    root::founding(files, time, entropy, identity, name, addresses);
                root.await.map(Some)
            }
            #[cfg(feature = "sim")]
            Self::Given(founding) => Ok(Some(founding)),
        }
    }
}

impl Endpoint {
    /// The endpoint of the node's one part of `port`.
    fn new(
        port: transport::Port,
        region: Opening,
        clock: env::clock::Clock,
        entropy: env::entropy::Entropy,
        net: env::net::Net,
        settings: Settings,
    ) -> Self {
        let bound = address::bound(&port);
        let mut parts = port.split(NonZeroUsize::MIN);
        Self {
            part: parts
                .pop()
                .expect("invariant: a port splits into the parts asked for"),
            bound,
            region,
            clock,
            entropy,
            net,
            settings,
        }
    }

    /// Where a program reaches the node of `identity`, whose transport [`Self::open`]
    /// makes with its private key.
    fn address(&self, identity: &identity::Identity) -> address::Address {
        address::Address {
            key: identity.key,
            public_key: identity.private_key.public(),
            bound: self.bound,
        }
    }

    /// Opens the node's transport on `pool` and `tasks` with `identity`, then, when the
    /// node has a region, the chunk store in directory [`directory::blob`] of `files`,
    /// and the mesh of that region over both, in directory [`directory::mesh`]. Gives
    /// the transport, also with the error of a founding, a store, or a mesh that did
    /// not open. A new root region takes the keys of its status at the mesh time of
    /// `time`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the transport refuses the settings.
    async fn open(
        self,
        identity: identity::Identity,
        files: env::files::Files,
        pool: Rc<block::Pool>,
        time: &clock::Reader,
        tasks: env::tasks::Tasks,
    ) -> Result<(Rc<transport::Transport>, Result<Option<mesh::Mesh>, Error>), Error>
    {
        let config = transport::Config {
            private_key: identity.private_key.clone(),
            message_bytes_max: self.settings.message_bytes_max,
            window_bytes: self.settings.window_bytes,
            streams_max: self.settings.streams_max,
            idle: self.settings.idle,
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: tasks.clone(),
            pool: Rc::clone(&pool),
        };
        let transport = transport::Transport::new(config, self.part);
        let transport = Rc::new(transport.map_err(Error::Transport)?);
        let founding = self.region.founding(
            &files,
            Rc::clone(&pool),
            time,
            &self.entropy,
            &identity,
        );
        let founding = match founding.await {
            Ok(Some(founding)) => founding,
            Ok(None) => return Ok((transport, Ok(None))),
            Err(error) => return Ok((transport, Err(error))),
        };
        let store = blob::Store::open(blob::Config {
            files: files.clone(),
            dir: directory::blob(),
            pool: Rc::clone(&pool),
            floor_bytes: self.settings.floor_bytes,
        });
        let store = match store.await {
            Ok(store) => store,
            Err(error) => return Ok((transport, Err(Error::Blob(error)))),
        };
        let config = mesh::Config {
            key: identity.key,
            private_key: identity.private_key,
            founding,
            files,
            dir: directory::mesh(),
            clock: self.clock,
            entropy: self.entropy,
            tasks,
            pool,
            transport: Rc::clone(&transport),
            store: Rc::new(store),
        };
        let mesh = mesh::Mesh::open(config).await;
        Ok((transport, mesh.map(Some).map_err(Error::Mesh)))
    }
}

impl Serve {
    /// Keeps the budgets ([`budget::keep`]), loads the node's identity
    /// ([`identity::load`]), and opens `endpoint`, then runs each task given with a hub
    /// over `home` that takes its definitions from the region's mesh
    /// ([`hub::Config::region`]) and its access rules from each spec in use
    /// ([`follow`]), runs each connector that the spec in use places on this node
    /// ([`connectors::Runs`]), and serves the node's port, until `guard` completes, the
    /// transport stops, or the mesh's group stops, by the rank of [`end`], with the
    /// address in the file `address` of `files` while it serves ([`address::hold`]). A
    /// transport or a group that ends it goes into `failed` before it drops the tasks
    /// given that still run. An `address` that was not written goes into `failed`
    /// before the tasks given drop uncalled. An `address` that was not written empty
    /// goes into `failed` after the tasks drop, unless an error is there. Before it
    /// returns, it drops the tasks, the hub, `home`, `guard`, the runs of the
    /// connectors, each session and stream future, the operations on the mesh, the
    /// mesh, and the transport, and waits for each task of the mesh to end, with a
    /// mesh, and for the transport to free the port. Unless the socket broke, the port
    /// is freed only after the hub lets go of the region, as [`hub::Hub::new`] states,
    /// and after each task that a connector's run spawned has ended. Runs no task or
    /// connector and takes no session when a shard did not open, or when the budgets
    /// were not kept, the identity did not load, the transport refused the settings,
    /// the mesh did not open, or `address` was not written, which goes into `failed`.
    #[expect(
        clippy::too_many_arguments,
        reason = "shard 0's parts, which its claim and its open give in turn"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "shard 0's serve, whose parts each open from the one before; a longer \
                  run answers the limit again"
    )]
    async fn run(
        self,
        endpoint: Endpoint,
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
        // After each buffer has opened, so a budget that gives a shard too little is
        // not kept.
        let loaded = load(&files, self.budget, &self.time, &endpoint.entropy);
        let Ok(identity) = loaded.await.map_err(&fail) else {
            return;
        };
        let (key, entropy) = (identity.key, endpoint.entropy.clone());
        let (clock, net) = (endpoint.clock.clone(), endpoint.net.clone());
        let address = endpoint.address(&identity);
        let (settings, vars, name) = (endpoint.settings, self.vars, self.name);
        let roots = self.roots;
        let shards = self.shards;
        let opened =
            endpoint.open(identity, files.clone(), pool, &self.time, tasks.clone());
        let Ok((transport, mesh)) = opened.await.map_err(&fail) else {
            return;
        };
        let (freed, inbox, time) = (transport.ended(), self.inbox, self.time);
        // Each part that holds the transport or the node's stop drops as this block
        // ends, on each path, or is a task of the mesh, awaited below, a task of the
        // hub, which ends as `hub::Hub::new` states, or a task that a connector's run
        // spawned, which its kind ends at the cancel. So the port is freed before
        // `lock` drops.
        let served = async move {
            let Ok(mesh) = mesh.map_err(&fail) else {
                return None;
            };
            let ended = mesh.as_ref().map(mesh::Mesh::ended);
            let secrets =
                secret::Resolver::new(vars, files.clone(), &directory::secrets());
            let serve = pin!(async move {
                // `hold` polls `serve` only once the file `address` holds the address.
                let trusted = trust::roots(&roots, mesh.is_some());
                let os::roots::Roots {
                    certificates,
                    failures,
                    source,
                } = roots;
                // Each local of this block lives until the node stops.
                drop(certificates);
                let text = source.to_string();
                drop(source);
                tracing::info!(%name, address = %address.bound, shards, "started");
                let region = mesh.clone().map(|mesh| hub::Region {
                    mesh,
                    transport: Rc::clone(&transport),
                });
                let hub = hub::Hub::new(hub::Config {
                    home,
                    interner,
                    tasks: tasks.clone(),
                    node: key,
                    time: time.clone(),
                    entropy: entropy.clone(),
                    region,
                });
                let (ops, followed) = if let Some(mesh) = mesh.as_ref() {
                    let member = mesh.member(key);
                    let member =
                        member.expect("invariant: a mesh opens only on a member");
                    let name = member.card.card().name.clone();
                    let kinds = Arc::new(kinds());
                    let ops = Rc::new(ops::Node::new(ops::Config {
                        mesh: mesh.clone(),
                        keys: channel_keys(&time, &entropy),
                        front_ends: front_ends(),
                        kinds: Arc::clone(&kinds),
                        hub: hub.clone(),
                        clock: clock.clone(),
                        name: name.clone(),
                        secrets: secrets.clone(),
                        roots: ops::roots::Failures::new(text, failures),
                    }));
                    let config = connector::supervisor::Config {
                        kinds,
                        clock: clock.clone(),
                        entropy,
                        net,
                        roots: trusted,
                        tasks: tasks.clone(),
                        hub: hub.clone(),
                        secrets,
                    };
                    let runs = connectors::Runs::new(config, name);
                    (Some(ops), Some(follow(mesh, hub.clone(), runs).await))
                } else {
                    drop((text, failures));
                    (None, None)
                };
                let handles = task::Handles { hub, ops };
                let port = route::accept(
                    transport,
                    handles.clone(),
                    clock,
                    tasks.clone(),
                    settings.header_wait,
                    settings.places_max,
                );
                let stop = until(guard, port, followed, fail);
                inbox.serve(handles, tasks, stop).await;
            });
            // `hold` leaves `serve` to drop here: after a failed write it still holds
            // the tasks given, and `join` must rank that error above a panic in a
            // task's drop. After a stop that failed the node, its error stands.
            if let Err(error) = address::hold(&files, &address, serve).await {
                failed.get_or_init(|| error);
            }
            ended
        };
        if let Some(ended) = served.await {
            ended.await;
        }
        freed.await;
    }
}

/// Keeps the budgets in `files` ([`budget::keep`]), then loads the node's identity
/// from them ([`identity::load`]).
async fn load(
    files: &env::files::Files,
    budget: Budget,
    time: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> Result<identity::Identity, Error> {
    budget::keep(files, budget).await?;
    identity::load(files, time, entropy).await
}

/// A new channel key: a UUIDv7 at mesh time read through `time`, with random bits
/// from `entropy`.
fn channel_key(
    time: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> types::channel::Key {
    // The time only orders keys, so a key before mesh time or 1970 has the time 0.
    // Not an edge or the midpoint of the interval: an edge moves back when the error
    // changes, and the midpoint when one edge stops at the end of the stamp range.
    let at = match time.status() {
        clock::Status::Synced(mesh) | clock::Status::Holdover(mesh, _) => {
            mesh.time().max(Stamp::EPOCH)
        }
        clock::Status::Unsynced(_) => Stamp::EPOCH,
    };
    let mut random = [0; 16];
    entropy.fill(&mut random);
    types::channel::Key::v7(at, u128::from_le_bytes(random))
}

/// The front ends of the config files that the operations read.
fn front_ends() -> ops::FrontEnds {
    ops::FrontEnds::new(
        "hcl",
        ops::FrontEnd {
            read: config_hcl::read,
        },
    )
}

/// Gives the key of each channel that the operations make ([`channel_key`]).
fn channel_keys(
    time: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> Box<dyn Fn() -> types::channel::Key> {
    let (time, entropy) = (time.clone(), entropy.clone());
    Box::new(move || channel_key(&time, &entropy))
}

/// Gives `hub` the access rules of the spec that `mesh` uses, and makes `runs` match
/// that spec ([`connectors::Runs::apply`]), with no wait: the watch gives the spec in
/// use at once ([`mesh::used::Watch::next`]). Then gives the future that does the same
/// for each later spec, and that ends with the stop of the group.
async fn follow(
    mesh: &mesh::Mesh,
    hub: hub::Hub,
    mut runs: connectors::Runs,
) -> impl Future<Output = mesh::Stopped> + use<> {
    let mut watch = mesh.watch_spec();
    let mut used = move |spec: mesh::used::Spec| {
        let tree = spec.definitions.iter();
        hub.set_rules(access::Rules::new([(Prefix::ROOT, tree)]));
        runs.apply(&spec.definitions);
    };
    let first = watch.next().await.map(&mut used);
    async move {
        if let Err(stopped) = first {
            return stopped;
        }
        loop {
            match watch.next().await {
                Ok(spec) => used(spec),
                Err(stopped) => return stopped,
            }
        }
    }
}

/// The connector kinds of this binary.
fn kinds() -> connector::kind::Table {
    connector::kind::Table::new()
        .with("influx", connector_influx::Kind::default())
        .with("modbus", connector_modbus::Kind::default())
        .with("opcua", connector_opcua::Kind::default())
}

/// Drives `runs`, with a mesh, which ends when the mesh's group stops. Ready when the
/// node stops (`guard`), the transport stops (`port`), or `runs` ends, by the rank of
/// [`end`]. A transport or a group that ends it goes into `fail` first.
async fn until(
    guard: Guard,
    port: impl Future<Output = transport::Error>,
    runs: Option<impl Future<Output = mesh::Stopped>>,
    fail: impl Fn(Error),
) {
    let (mut guard, mut port) = (pin!(guard), pin!(port));
    let mut runs = pin!(runs);
    poll_fn(|cx| {
        let group = match runs.as_mut().as_pin_mut() {
            Some(runs) => runs.poll(cx),
            None => Poll::Pending,
        };
        let ended = end(guard.as_mut().poll(cx), port.as_mut().poll(cx), group);
        // Set before the tasks drop, so that `join` ranks it above a panic in a
        // task's drop.
        ended.map(|error| error.map_or((), &fail))
    })
    .await;
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

/// Why a node failed, or why [`name`] gave no name or [`budget`] could not read the
/// budgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A shard or the thread `log` could not start, or a shard could not pin.
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
    /// A file call that locks the data directory, reads or records its shard count or
    /// the node's name or its budgets, reads or writes the node's key or the first
    /// admin's key, or opens, writes, or removes the file `address`, failed. Also
    /// [`env::files::Error::Exists`] when `create_key` finds a file at `node.key`, with
    /// no file call that failed.
    /// [`env::files::Error::Busy`] on `lock` is another node that runs on the data
    /// directory.
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
    /// The pool budget gives one of `cores` shards a pool whose reservation is more
    /// than `usize::MAX` bytes, more address space than this host has. No shard
    /// started.
    Pool {
        /// The pool budget that was given.
        pool: types::byte::Size,
        /// The count of shards.
        cores: usize,
    },
    /// The transport of the node's port stopped, as when the OS breaks its socket, or
    /// refused a value of [`Config::settings`] ([`transport::Error::Config`]) before
    /// the node took a session. The node stops.
    Transport(transport::Error),
    /// The mesh did not open, or the founding that the data directory stores did not
    /// read. A node that gives it took no session.
    Mesh(mesh::Error),
    /// The group of the node's mesh stopped, as when a write of its log fails. The
    /// node stops.
    Group(mesh::Stopped),
    /// The chunk store did not open. The node took no session.
    Blob(blob::Error),
    /// The file `node.key` in the data directory is not a key that a node wrote:
    /// another length, also no bytes, or another tag or checksum, also 100 zero bytes.
    /// The node took no session and does not write over the file, because a new key is
    /// a new node to its region.
    Key,
    /// At a start that founds the root region, the file `admin.key` in the data
    /// directory is not a key that a node wrote: another length, also no bytes, or
    /// another tag or checksum. The node took no session and does not write over the
    /// file.
    Admin,
    /// [`Config::listen`] has an IP at which no peer reaches one node: a multicast IP
    /// or the IPv4 broadcast IP, also as an IPv4-mapped IPv6 address. The node bound no
    /// port, and no shard started.
    Listen {
        /// The address that the node refused.
        listen: SocketAddr,
    },
    /// The node's port did not bind. The node had claimed the data directory, and
    /// opened no buffer.
    Port {
        /// The address of the bind.
        listen: SocketAddr,
        /// Why it did not bind.
        error: env::net::Error,
    },
    /// The file `name` in the data directory is not a name that a node wrote. The node
    /// does not write over it.
    Name,
    /// The data directory holds the node `stored`, and the start gave `given`. The
    /// stored name is the one in the file `name`, else the one in the node's card in
    /// the founding that the data directory stores.
    Renamed {
        /// The name in the data directory.
        stored: types::name::Name,
        /// The name the start gave.
        given: types::name::Name,
    },
    /// The data directory holds no node name and stores no founding, and the start gave
    /// no name.
    Unnamed,
    /// The data directory has the directory of a mesh, and the file `node.key` is not
    /// there, or, with no file `name`, it holds a key with no card in the founding of
    /// its region. A start that gives it wrote no file `name` and no new `node.key`,
    /// because a new key is a new node to its region.
    NoCard,
    /// The start gave `name`, which has a segment that starts with `@`. A region
    /// refuses it, so no shard started and the data directory keeps no name that its
    /// founding refuses.
    Reserved {
        /// The name the start gave.
        name: types::name::Name,
    },
    /// The start gave `name`, which is longer than 191 bytes: the most that leaves room
    /// for the name of each status channel of a node, `<name>.<status>`. No shard
    /// started, and the data directory keeps no such name.
    Long {
        /// The name the start gave.
        name: types::name::Name,
    },
    /// The file `budget` in the data directory does not hold budgets that a node
    /// wrote. The node does not write over it.
    Budget,
}

impl Error {
    /// The failed file call that caused this error, or `None` when no file call
    /// caused it.
    #[must_use]
    pub fn files(&self) -> Option<&env::files::Error> {
        match self {
            Self::Directory(error) => Some(error),
            Self::Buffer { error, .. } => cause::buffer(error),
            Self::Blob(error) => cause::blob(error),
            Self::Mesh(error) => cause::mesh(error),
            Self::Group(stopped) => cause::stopped(stopped),
            Self::Start(_)
            | Self::Panicked(_)
            | Self::Memory { .. }
            | Self::Pool { .. }
            | Self::Shards { .. }
            | Self::Disk { .. }
            | Self::Transport(_)
            | Self::Key
            | Self::Admin
            | Self::Listen { .. }
            | Self::Port { .. }
            | Self::Name
            | Self::Renamed { .. }
            | Self::Unnamed
            | Self::NoCard
            | Self::Reserved { .. }
            | Self::Long { .. }
            | Self::Budget => None,
        }
    }
}

impl fmt::Display for Error {
    #[expect(clippy::too_many_lines, reason = "one arm for each error")]
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
                write!(f, "cannot use the data directory: {error}")
            }
            Self::Disk { disk, cores, min } => write!(
                f,
                "the disk budget {disk} holds no ring on each of {cores} shards; it \
                 needs at least {min}"
            ),
            Self::Pool { pool, cores } => write!(
                f,
                "the pool budget {pool} gives one of {cores} shards a pool that needs \
                 more address space than this host has"
            ),
            Self::Transport(error @ transport::Error::Config { .. }) => {
                write!(f, "the node's transport refused its settings: {error}")
            }
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
                "the file `node.key` in the data directory is not a node key; restore it \
                 from a backup of this node",
            ),
            Self::Admin => f.write_str(
                "the file `admin.key` in the data directory is not an admin key; \
                 restore it from a backup, or remove it to make a new admin",
            ),
            Self::Listen { listen } => write!(
                f,
                "cannot bind the node's port at {listen}: a multicast or broadcast IP \
                 is not an address of a host"
            ),
            Self::Port { listen, error } => {
                write!(f, "cannot bind the node's port at {listen}: {error}")
            }
            Self::Name => f.write_str(
                "the file `name` in the data directory is not a node name; remove it, and \
                 start the node with its name",
            ),
            Self::Renamed { stored, given } => write!(
                f,
                "the data directory holds the node {stored}, not {given}; give \
                 {stored}, or another data directory"
            ),
            Self::Unnamed => f.write_str(
                "the data directory holds no node name; give the node a name",
            ),
            Self::NoCard => f.write_str(
                "the data directory has the directory of a mesh, and the file \
                 `node.key` is not there or holds a key that the founding of its \
                 region does not hold; restore `node.key` from a backup of this node, \
                 or give another data directory",
            ),
            Self::Reserved { name } => write!(
                f,
                "the name {name} has a segment that starts with `@`, which is reserved \
                 for Foundation; give another name"
            ),
            Self::Long { name } => write!(
                f,
                "the name {name} is longer than {} bytes, the most that leaves room for \
                 the name of each status channel of the node",
                name::MAX_BYTES
            ),
            Self::Budget => f.write_str(
                "the file `budget` in the data directory does not hold budgets that a \
                 node wrote; remove it, and the next start writes it again",
            ),
        }
    }
}

impl std::error::Error for Error {}
