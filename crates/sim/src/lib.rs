//! Simulates the `env` seams with a deterministic scheduler and fault injection; ships
//! behind a feature.
//!
//! One [`Sim`] runs every node of a mesh on the calling thread. A seeded scheduler
//! polls one ready task at a time, and true time moves only when no task is ready: to
//! the next timer, arrival, or end of a file call. Datagrams cross [`link`]s with
//! delay and faults, and bytes cross serial [`line`](mod@line)s at the line rate.
//! The same seed and the same calls give the same run.
//!
//! A task panic becomes [`Error::Panicked`] only in a build that unwinds on panic, as
//! tests do. A build with `panic = "abort"` ends the process at the panic.

pub mod line;
pub mod link;
pub mod name;
pub mod node;
pub mod shard;

mod chance;
mod disk;
mod drivers;
mod files;
mod net;
mod serial;
mod state;
#[cfg(test)]
mod tests;

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::files::Files;
use crate::net::Network;
use crate::node::Node;
use crate::serial::Serial;
use crate::state::{Ended, Futures, Next, Outcome, Shared, Start, State, lock};

/// Settings for one run. Build it with `..Config::default()`: fields get added.
///
/// ```
/// let config = sim::Config { seed: 0x2a, ..sim::Config::default() };
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// The replay value: the same seed and the same calls give the same run.
    pub seed: u64,
    /// The most steps in one call to [`Sim::run`] or [`Sim::run_for`]. A step polls
    /// one task, or moves true time to the next timer, arrival, end of a file call, or
    /// end of a pause.
    pub steps_max: u64,
    /// The link from each node to each node, itself too, until [`Sim::link`] sets
    /// another.
    pub link: link::Config,
}

impl Default for Config {
    /// Seed 0, ten million steps, and the default link.
    fn default() -> Self {
        Self {
            seed: 0,
            steps_max: 10_000_000,
            link: link::Config::default(),
        }
    }
}

/// A deterministic run of one or more nodes on the calling thread. Make nodes with
/// [`Sim::node`], start their shards and threads through the `env` handles, then
/// drive the run with [`Sim::run`], [`Sim::run_for`], or [`Sim::run_on`].
///
/// ```
/// use types::time::{Monotonic, Span};
///
/// let mut sim = sim::Sim::new(sim::Config::default());
/// let node = sim.node(sim::node::Config::default());
/// let clock = node.clock();
/// let config = env::shards::Config { name: "shard-0".into(), core: Some(0) };
/// let handle = node.shards().start(config, move |_tasks| async move {
///     clock.sleep(Span::SECOND).await;
/// });
/// sim.run().unwrap();
/// handle.unwrap().join().unwrap();
/// ```
///
/// # Panics
///
/// The drop of a `Sim` ends every thread as [`Sim::crash`] does, so it drops each task
/// and each thread that has not started, and a task or thread that one of these drops
/// starts is dropped in that drop. When one of these drops panics, the others still
/// run, and then the drop panics once with each message, as [`Error::Panicked`] gives
/// them: those of the tasks first, then those of the threads, each in start order. It
/// does not panic while the thread already panics.
pub struct Sim {
    config: Config,
    shared: Shared,
    futures: Rc<RefCell<Futures>>,
    scheduler: Rng,
    /// Gives each new node its entropy stream.
    streams: Rng,
}

impl Sim {
    /// Makes a run with no nodes, at true time zero.
    ///
    /// # Panics
    ///
    /// When `config.link` has a negative span or a chance outside 0 to 1.
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the epoch is an opaque base: only differences from it are read"
    )]
    pub fn new(config: Config) -> Self {
        config.link.check();
        let mut streams = Rng::from_seed(config.seed);
        let scheduler = Rng::from_seed(streams.next_u64());
        let net = Network::new(config.link, Rng::from_seed(streams.next_u64()));
        let files = Files::new(Rng::from_seed(streams.next_u64()));
        let serial = Serial::new(Rng::from_seed(streams.next_u64()));
        let state = State::new(Instant::now(), net, files, serial);
        Self {
            config,
            shared: Arc::new(Mutex::new(state)),
            futures: Rc::default(),
            scheduler,
            streams,
        }
    }

    /// Adds a node. Its clocks read the values in `config` now, and its entropy is
    /// its own stream from the seed.
    pub fn node(&mut self, config: node::Config) -> Node {
        let entropy = Rng::from_seed(self.streams.next_u64());
        let node = lock(&self.shared).add(config, entropy);
        let shared = Arc::clone(&self.shared);
        Node(drivers::Node { shared, node })
    }

    /// Sets the link from `from` to `to`, one way, for the datagrams sent from now on.
    ///
    /// # Panics
    ///
    /// When a node belongs to another run, or when `config` has a negative span or a
    /// chance outside 0 to 1.
    pub fn link(&mut self, from: &Node, to: &Node, config: link::Config) {
        let (from, to) = (self.own(from), self.own(to));
        config.check();
        lock(&self.shared).net().link(from, to, config);
    }

    /// Makes each lookup of `host` that starts from now on, on any node, go as
    /// `config` says. As in DNS, a name matches in any ASCII case and with or
    /// without one final dot. A name that no call gave has no address.
    ///
    /// ```
    /// let mut sim = sim::Sim::new(sim::Config::default());
    /// let historian = "10.0.0.2".parse().expect("an address");
    /// let answer = sim::name::Answer::Addresses(vec![historian]);
    /// let config = sim::name::Config { answer, ..sim::name::Config::default() };
    /// sim.name("historian.local", config);
    /// ```
    ///
    /// # Panics
    ///
    /// When `config.delay` is negative, or when `host` is an IP literal, which
    /// [`env::net::Net::resolve`] gives with no lookup.
    pub fn name(&mut self, host: &str, config: name::Config) {
        config.check(host);
        lock(&self.shared).net().name(host, config);
    }

    /// Sets the line that joins port `a_path` of `a` and port `b_path` of `b`, for the
    /// bytes sent from now on. An open of either path on its node opens that end.
    ///
    /// - Byte `k` of a run of bytes sent back to back, from 1, arrives
    ///   [`rate.span(k)`](types::time::Rate::span) after the run starts, where
    ///   `rate` is the [`rate`](env::serial::Settings::rate) of the sender's
    ///   settings.
    /// - A byte arrives intact only when both ends have the same settings. Otherwise
    ///   it arrives as a random byte.
    /// - Each line draws its faults from its own stream as each byte is sent.
    ///   Setting the line again keeps its ends and draws a new stream.
    ///
    /// # Panics
    ///
    /// When a node belongs to another run, when the two ends are one, when an end
    /// is on another line, or when `config` has a chance outside 0 to 1.
    pub fn line(
        &mut self,
        a: &Node,
        a_path: &Path,
        b: &Node,
        b_path: &Path,
        config: line::Config,
    ) {
        let a = (self.own(a), a_path.to_path_buf());
        let b = (self.own(b), b_path.to_path_buf());
        let (node, path) = (a.0, a.1.display());
        assert!(
            a != b,
            "port {path} of node {node} cannot be both ends of a line"
        );
        config.check();
        let joined = lock(&self.shared).serial().join(a, b, config);
        if let Err((node, path)) = joined {
            let path = path.display();
            panic!("port {path} of node {node} is on another line");
        }
    }

    /// Crashes `node` now, between runs. Each thread of the node ends at once: no task
    /// of it polls again, its futures and its threads that have not run drop, so its
    /// sockets and ports close and its timers stop, and [`env::thread::Handle::join`]
    /// on one of them panics. A thread that one of these drops starts on the node also
    /// ends in the crash and never runs. Each file call of the node ends, and each file
    /// handle and serial port closes, leaked ones too. The blocks of the calls go back
    /// to their pools. The node keeps its disk and its addresses: start new threads on
    /// it to restart it.
    ///
    /// # Panics
    ///
    /// - When `node` belongs to another run.
    /// - When the drop of a future or of a thread that has not run panics. The crash
    ///   still ends, and the panic gives each message as [`Error::Panicked`] does:
    ///   those of the futures first, then those of the threads, each in start order.
    pub fn crash(&mut self, node: &Node, crash: Crash) {
        let node = self.own(node);
        let wakers = {
            let mut state = lock(&self.shared);
            let now = state.now();
            state.net().crash(now, node, crash)
        };
        let panics = self.stop(|key| key == node);
        drop(wakers);
        let ended = lock(&self.shared).crash(node, crash);
        drop(ended);
        assert!(panics.is_empty(), "{}", panics.join(THEN));
    }

    /// A hash of every scheduler pick, every packet event, every byte arrival on a
    /// line, and every end of a file call so far: the time, addresses, length, and
    /// fate of a packet, the kind of a TCP segment, the time, end, and fate of a
    /// byte, and the time, kind, and success of a call, never the bytes. In one
    /// build, the same seed and the same calls give the same digest.
    #[must_use]
    pub fn digest(&self) -> u64 {
        lock(&self.shared).digest()
    }

    /// Runs until every thread of every node has ended.
    ///
    /// # Errors
    ///
    /// - [`Error::Panicked`] when a task panics, in a poll or in the drop of its
    ///   future. The run stops there, the thread's other futures drop, and the
    ///   thread's [`env::thread::Handle::join`] returns
    ///   [`env::thread::Panicked`].
    /// - [`Error::Steps`] past [`Config::steps_max`] steps.
    /// - [`Error::Stuck`] when threads remain but nothing can run again: no task is
    ///   ready on a node that runs, and no timer, arrival, file call, or pause ends
    ///   before the end of true time.
    ///
    /// # Panics
    ///
    /// When a TCP segment, or the drop of a stream, meets a case that sim does not
    /// simulate yet, as [`Node::net`](node::Node::net) lists. A case met between
    /// runs, as in a drop or a crash, panics at the start of the next run.
    pub fn run(&mut self) -> Result<(), Error> {
        self.drive(None)
    }

    /// Starts a shard named `run_on`, with no core, on `node` that runs `body`, runs
    /// until every thread of every node has ended, and returns what `body` gave.
    /// `body` gets the node and the shard's tasks. Every call names its shard
    /// `run_on`.
    ///
    /// ```
    /// let mut sim = sim::Sim::new(sim::Config::default());
    /// let node = sim.node(sim::node::Config::default());
    /// let now = sim.run_on(&node, |node, _tasks| async move { node.clock().now() });
    /// assert_eq!(now, Ok(sim::node::Config::default().monotonic));
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Sim::run`]. [`Error::Panicked`] with thread `run_on` when `body` panics.
    ///
    /// # Panics
    ///
    /// When `node` belongs to another run.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a shard with no core gets no fault, and the run ends after the shard"
    )]
    pub fn run_on<T, F>(
        &mut self,
        node: &Node,
        body: impl FnOnce(Node, env::tasks::Tasks) -> F + Send + 'static,
    ) -> Result<T, Error>
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        self.own(node);
        let out = Arc::new(Mutex::new(None));
        let (slot, own) = (Arc::clone(&out), node.clone());
        let config = env::shards::Config {
            name: "run_on".into(),
            core: None,
        };
        let start = node.shards().start(config, move |tasks| async move {
            let value = body(own, tasks).await;
            *slot
                .lock()
                .expect("invariant: nothing panics under the lock") = Some(value);
        });
        drop(start.expect("invariant: a shard with no core gets no fault"));
        self.run()?;
        let value = out
            .lock()
            .expect("invariant: nothing panics under the lock")
            .take();
        Ok(value.expect("invariant: the run ends after its first task"))
    }

    /// Runs until true time has moved by `span`; a negative span runs as zero. Threads
    /// that still wait then are fine.
    ///
    /// True time ends where the first clock of a node can count no further: the
    /// monotonic clock at `u64::MAX` nanoseconds, or the wall clock in 2262. A timer
    /// or arrival past that end comes only if a wall step moves the end past it. A
    /// wall step in the run can bring the end nearer; the run then stops there.
    ///
    /// # Errors
    ///
    /// [`Error::Panicked`] and [`Error::Steps`], as [`Sim::run`].
    ///
    /// # Panics
    ///
    /// When `span` reaches past the end of true time, and as [`Sim::run`].
    pub fn run_for(&mut self, span: Span) -> Result<(), Error> {
        let span = span.max(Span::ZERO);
        let end = lock(&self.shared).after(span);
        let Some(end) = end else {
            panic!("run_for({span}) passes the end of true time")
        };
        self.drive(Some(end))
    }

    /// Takes steps until true time `end`, or until no task is ready and no timer
    /// waits.
    fn drive(&mut self, end: Option<Monotonic>) -> Result<(), Error> {
        let mut steps = self.config.steps_max;
        loop {
            let mut state = lock(&self.shared);
            let (yet, next) = (state.net().yet(), state.next(end));
            drop(state);
            if let Some(yet) = yet {
                panic!("{yet}");
            }
            let Some(next) = next else { break };
            steps = steps.checked_sub(1).ok_or(Error::Steps {
                max: self.config.steps_max,
                seed: self.config.seed,
            })?;
            match next {
                Next::Poll => self.step()?,
                Next::Advance(at) => {
                    let (wakers, orphans) = lock(&self.shared).advance(at);
                    wakers.into_iter().for_each(Waker::wake);
                    drop(orphans);
                }
            }
        }
        let threads = lock(&self.shared).live();
        if end.is_some() || threads.is_empty() {
            return Ok(());
        }
        let seed = self.config.seed;
        Err(Error::Stuck { threads, seed })
    }

    /// Polls one ready task, picked at random. A panic in the poll, or in a drop of
    /// the futures that end with the task, ends the task's thread and the run.
    fn step(&mut self) -> Result<(), Error> {
        let (task, thread, start) = lock(&self.shared).pick(&mut self.scheduler);
        let run = panic::catch_unwind(AssertUnwindSafe(|| {
            if self.poll(task, thread, start).is_pending() {
                return Vec::new();
            }
            let done = lock(&self.shared).finish(task, thread);
            self.drop_ended(done)
        }));
        lock(&self.shared).release();
        if matches!(&run, Ok(panics) if panics.is_empty()) {
            return Ok(());
        }
        let name = lock(&self.shared).name(thread);
        let panicked = env::thread::Panicked { name: name.clone() };
        let ended = lock(&self.shared).end(thread, Outcome::Done(Err(panicked)));
        // The payload drops after the thread ends, as the thread's futures do.
        let mut panics = run.unwrap_or_else(messages);
        panics.extend(self.drop_ended(ended));
        Err(Error::Panicked {
            thread: name,
            message: panics.join(THEN),
            seed: self.config.seed,
        })
    }

    /// Polls `task` once. A thread's first task makes its future here, on the
    /// simulated thread. A future whose poll panics goes back to the store, so that
    /// it drops as the others do.
    fn poll(&self, task: u64, thread: u64, start: Option<Start>) -> Poll<()> {
        if let Some(start) = start {
            let future = match start {
                Start::Shard(main) => main(env::tasks::Tasks::new(drivers::Tasks {
                    shared: Arc::clone(&self.shared),
                    futures: Rc::clone(&self.futures),
                    thread,
                })),
                Start::Body(body) => body(),
            };
            self.futures.borrow_mut().insert(&self.shared, task, future);
        }
        let (mut future, waker) = self.futures.borrow_mut().take(task);
        let mut context = Context::from_waker(&waker);
        let poll = panic::catch_unwind(AssertUnwindSafe(|| {
            future.as_mut().poll(&mut context)
        }));
        match poll {
            Ok(Poll::Ready(())) => Poll::Ready(()),
            Ok(Poll::Pending) => {
                self.futures.borrow_mut().put(task, future);
                Poll::Pending
            }
            Err(payload) => {
                self.futures.borrow_mut().put(task, future);
                panic::resume_unwind(payload)
            }
        }
    }

    /// The index of `node`.
    ///
    /// # Panics
    ///
    /// When `node` belongs to another run.
    fn own(&self, node: &Node) -> usize {
        let own = Arc::ptr_eq(&node.0.shared, &self.shared);
        assert!(own, "{node:?} belongs to another sim");
        node.0.node
    }

    /// Ends each live thread of the nodes whose index `stopped` picks, as
    /// [`State::stop`] does, and drops their tasks, then the threads that have not
    /// started, outside the lock. Returns the message of each drop that panicked, in
    /// order.
    fn stop(&self, stopped: impl Fn(usize) -> bool) -> Vec<String> {
        let (ended, starts) = lock(&self.shared).stop(stopped);
        let mut panics = self.drop_ended(ended);
        panics.extend(drop_each(starts));
        panics
    }

    /// Drops the timer wakers of `ended`, and the futures of its tasks outside the
    /// borrow, since a drop may spawn. Returns the message of each drop that panicked,
    /// in order.
    fn drop_ended(&self, ended: Ended) -> Vec<String> {
        drop(ended.timers);
        let futures = self.futures.borrow_mut().remove(&ended.tasks);
        drop_each(futures)
    }
}

/// Drops each item on its own, as a second panic in one unwind aborts the process.
/// Returns the [`messages`] of each drop that panicked, in order.
fn drop_each<T>(items: impl IntoIterator<Item = T>) -> Vec<String> {
    (items.into_iter())
        .filter_map(|item| panic::catch_unwind(AssertUnwindSafe(|| drop(item))).err())
        .flat_map(messages)
        .collect()
}

/// The Linux code for an I/O error (`EIO`).
const EIO: i32 = 5;

/// The Linux code for a failure that may pass (`EAGAIN`).
const EAGAIN: i32 = 11;

/// The most payloads that [`messages`] drops in one chain.
const CHAIN: usize = 16;

/// What joins the messages of two panics.
const THEN: &str = ", then a drop panicked: ";

/// The message of a panic payload.
fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a payload that is not a string".to_owned()
    }
}

/// The message of `payload`, then of each panic in the drop of the payload before
/// it. It drops at most [`CHAIN`] payloads, each in a catch, and forgets the payload
/// past them, so that a drop that always panics cannot hang the run.
fn messages(payload: Box<dyn Any + Send>) -> Vec<String> {
    let mut messages = vec![message(&*payload)];
    let mut next = payload;
    for _ in 0..CHAIN {
        match panic::catch_unwind(AssertUnwindSafe(|| drop(next))) {
            Ok(()) => return messages,
            Err(payload) => {
                messages.push(message(&*payload));
                next = payload;
            }
        }
    }
    #[expect(clippy::mem_forget, reason = "its drop may panic again")]
    mem::forget(next);
    messages
}

impl Drop for Sim {
    /// Stops every node first, so a task that a drop spawns or a thread that it starts
    /// is born ended and dropped in that drop.
    fn drop(&mut self) {
        let panics = self.stop(|_| true);
        assert!(
            panics.is_empty() || std::thread::panicking(),
            "{}",
            panics.join(THEN)
        );
    }
}

impl fmt::Debug for Sim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sim")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// How a node crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crash {
    /// The process dies, as on a kill or a panic with `panic = "abort"`. The disk
    /// keeps each call that ended. Each file call in flight takes effect at the
    /// crash, as if its future dropped, so a write keeps any subset of its sectors.
    /// A [`Mode::Create`](env::files::Mode::Create) open in flight that makes a file
    /// leaves it whole or with no bytes. Each TCP stream and listener drops.
    Process,
    /// The machine loses power and boots again.
    ///
    /// - Each [`SECTOR`](env::files::SECTOR) of a file keeps the bytes that a sync
    ///   made durable, or its bytes after any one write on it since then, a write
    ///   in flight too. Where writes in flight at once overlap, it can keep a part
    ///   of one of them.
    /// - Each directory goes back to its entries when its last `sync_dir` ended,
    ///   and what those entries no longer reach is gone.
    /// - A [`Mode::Create`](env::files::Mode::Create) open in flight that makes a
    ///   file leaves no file, or a file with no bytes whose entry is durable.
    /// - Other file calls in flight have no effect.
    /// - The monotonic clock reads [`node::Config::monotonic`] again. The wall
    ///   clock runs on.
    /// - Each packet that waits to be sent on a link from the node is lost.
    /// - Each TCP stream and listener ends with no segment, so a peer gets an RST
    ///   only when it sends.
    Power,
}

/// Why a run failed. Each variant carries the seed that replays it, and its message
/// ends with "replay with seed 0x..".
///
/// ```
/// let e = sim::Error::Steps { max: 100, seed: 0x2a };
/// assert_eq!(e.to_string(), "the run passed 100 steps; replay with seed 0x2a");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A task panicked, which ended its thread.
    Panicked {
        /// The thread's name.
        thread: String,
        /// The panic message. When drops of the thread's futures or of a panic
        /// payload panic after it, the message of each follows, after ", then a drop
        /// panicked: ".
        message: String,
        /// The seed that replays the run.
        seed: u64,
    },
    /// The run passed [`Config::steps_max`] steps.
    Steps {
        /// The limit.
        max: u64,
        /// The seed that replays the run.
        seed: u64,
    },
    /// Threads remain, but nothing can wake them.
    Stuck {
        /// The names of the waiting threads, in start order.
        threads: Vec<String>,
        /// The seed that replays the run.
        seed: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seed = match self {
            Self::Panicked {
                thread,
                message,
                seed,
            } => {
                write!(f, "thread {thread} panicked: {message}")?;
                seed
            }
            Self::Steps { max, seed } => {
                write!(f, "the run passed {max} steps")?;
                seed
            }
            Self::Stuck { threads, seed } => {
                let threads = threads.join(", ");
                write!(f, "threads {threads} wait, and nothing can wake them")?;
                seed
            }
        };
        write!(f, "; replay with seed {seed:#x}")
    }
}

impl std::error::Error for Error {}
