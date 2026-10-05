//! Simulates the `env` seams with a deterministic scheduler and fault injection; ships
//! behind a feature.
//!
//! One [`Sim`] runs every node of a mesh on the calling thread. A seeded scheduler
//! polls one ready task at a time, and true time moves only when no task is ready: to
//! the next timer, datagram arrival, or end of a file call. Datagrams cross [`link`]s
//! with delay and faults. The same seed and the same calls give the same run.
//!
//! A task panic becomes [`Error::Panicked`] only in a build that unwinds on panic, as
//! tests do. A build with `panic = "abort"` ends the process at the panic.

pub mod link;
pub mod node;
pub mod shard;

mod chance;
mod disk;
mod drivers;
mod files;
mod net;
mod state;
#[cfg(test)]
mod tests;

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::files::Files;
use crate::net::Network;
use crate::node::Node;
use crate::state::{Futures, Next, Outcome, Shared, Start, State, lock};

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
/// drive the run with [`Sim::run`] or [`Sim::run_for`].
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
        let state = State::new(Instant::now(), net, files);
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

    /// Crashes `node` now, between runs. Each thread of the node ends at once: no
    /// task of it polls again, its futures and its threads that have not run drop,
    /// so its sockets close and its timers stop, and
    /// [`env::thread::Handle::join`] on one of them panics. The node keeps its disk
    /// and its addresses: start new threads on it to restart it.
    ///
    /// # Panics
    ///
    /// - When `node` belongs to another run.
    /// - When the drop of a future panics. The crash still ends, and the panic gives
    ///   each message as [`Error::Panicked`] does.
    pub fn crash(&mut self, node: &Node, crash: Crash) {
        let node = self.own(node);
        let (tasks, starts) = lock(&self.shared).crash(node);
        let panics = self.drop_futures(&tasks);
        drop(starts);
        if crash == Crash::Power {
            let orphans = lock(&self.shared).cut_power(node);
            drop(orphans);
        }
        assert!(panics.is_empty(), "{}", panics.join(THEN));
    }

    /// A hash of every scheduler pick, every datagram event, and every end of a file
    /// call so far: the time, addresses, length, and fate of a datagram, and the
    /// time, kind, and success of a call, never the bytes. In one build, the same
    /// seed and the same calls give the same digest.
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
    pub fn run(&mut self) -> Result<(), Error> {
        self.drive(None)
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
    /// When `span` reaches past the end of true time.
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
            let next = lock(&self.shared).next(end);
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
            self.drop_futures(&done)
        }));
        lock(&self.shared).release();
        let mut panics = run.unwrap_or_else(|payload| vec![message(&*payload)]);
        if panics.is_empty() {
            return Ok(());
        }
        let name = lock(&self.shared).name(thread);
        let panicked = env::thread::Panicked { name: name.clone() };
        let tasks = lock(&self.shared).end(thread, Outcome::Done(Err(panicked)));
        panics.extend(self.drop_futures(&tasks));
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

    /// Drops the futures of `tasks`, outside the borrow, since a drop may spawn.
    /// Returns the message of each drop that panicked, in order. Each future drops on
    /// its own, as a second panic in one unwind aborts the process.
    fn drop_futures(&self, tasks: &[u64]) -> Vec<String> {
        let futures = self.futures.borrow_mut().remove(tasks);
        (futures.into_iter())
            .filter_map(|future| {
                panic::catch_unwind(AssertUnwindSafe(|| drop(future))).err()
            })
            .map(|payload| message(&*payload))
            .collect()
    }
}

/// What joins the messages of two panics of one thread.
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

impl Drop for Sim {
    /// Drops every task and every thread that has not started. They may hold handles
    /// to the run, so they are dropped outside its lock.
    fn drop(&mut self) {
        let starts = lock(&self.shared).unstarted();
        let futures = self.futures.borrow_mut().clear();
        drop(starts);
        drop(futures);
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
    /// keeps each call that ended, and each file call in flight still ends, as if
    /// its future dropped. Until then, [`Node::files`] gives `Error::Locked`.
    Process,
    /// The machine loses power and boots again.
    ///
    /// - Each 512-byte sector of a file keeps the bytes that a sync made durable,
    ///   or the bytes of any one write on it since then, a write in flight too.
    /// - Each directory goes back to its entries when its last `sync_dir` ended,
    ///   and what those entries no longer reach is gone.
    /// - Other file calls in flight have no effect.
    /// - The monotonic clock reads [`node::Config::monotonic`] again. The wall
    ///   clock runs on.
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
        /// The panic message. When drops of the thread's futures panic after it, the
        /// message of each follows, after ", then a drop panicked: ".
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
