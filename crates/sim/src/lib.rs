//! Simulates the `env` seams with a deterministic scheduler and fault injection; ships
//! behind a feature.
//!
//! One [`Sim`] runs every node of a mesh on the calling thread. A seeded scheduler
//! polls one ready task at a time, and true time moves only when no task is ready: to
//! the next timer. The same seed and the same calls give the same run.
//!
//! A task panic becomes [`Error::Panicked`] only in a build that unwinds on panic, as
//! tests do. A build with `panic = "abort"` ends the process at the panic.

pub mod node;

mod drivers;
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

use crate::node::Node;
use crate::state::{Futures, Next, Shared, Start, State, lock};

/// Settings for one run. Build it with `..Config::default()`: fields get added.
///
/// ```
/// let config = sim::Config { seed: 0x2a, ..sim::Config::default() };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The replay value: the same seed and the same calls give the same run.
    pub seed: u64,
    /// The most steps in one call to [`Sim::run`] or [`Sim::run_for`]. A step polls
    /// one task or moves true time to the next timer.
    pub steps_max: u64,
}

impl Default for Config {
    /// Seed 0 and ten million steps.
    fn default() -> Self {
        Self {
            seed: 0,
            steps_max: 10_000_000,
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
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the epoch is an opaque base: only differences from it are read"
    )]
    pub fn new(config: Config) -> Self {
        let mut streams = Rng::from_seed(config.seed);
        let state = State::new(Instant::now());
        Self {
            config,
            shared: Arc::new(Mutex::new(state)),
            futures: Rc::default(),
            scheduler: Rng::from_seed(streams.next_u64()),
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

    /// Runs until every thread of every node has ended.
    ///
    /// # Errors
    ///
    /// - [`Error::Panicked`] when a task panics. The run stops there, and the
    ///   thread's [`env::threads::Handle::join`] returns
    ///   [`env::threads::Error::Panicked`].
    /// - [`Error::Steps`] past [`Config::steps_max`] steps.
    /// - [`Error::Stuck`] when threads remain but no task is ready and no timer
    ///   waits.
    pub fn run(&mut self) -> Result<(), Error> {
        self.drive(None)
    }

    /// Runs until true time has moved by `span`; a negative span runs as zero. Threads
    /// that still wait then are fine.
    ///
    /// True time ends where the first clock of a node can count no further: the
    /// monotonic clock at `u64::MAX` nanoseconds, or the wall clock in 2262. A timer
    /// past that end never fires.
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
                Next::Fire(due) => {
                    let wakers = lock(&self.shared).advance(due);
                    wakers.into_iter().for_each(Waker::wake);
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
            if self.poll(task, thread, start).is_ready() {
                let done = lock(&self.shared).finish(task, thread);
                self.drop_futures(&done);
            }
        }));
        lock(&self.shared).release();
        let Err(payload) = run else { return Ok(()) };
        let name = lock(&self.shared).name(thread);
        let panicked = env::threads::Error::Panicked { name: name.clone() };
        let tasks = lock(&self.shared).end(thread, Err(panicked));
        self.drop_futures(&tasks);
        Err(Error::Panicked {
            thread: name,
            message: message(&*payload),
            seed: self.config.seed,
        })
    }

    /// Polls `task` once. A thread's first task makes its future here, on the
    /// simulated thread.
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
        let poll = future.as_mut().poll(&mut Context::from_waker(&waker));
        if poll.is_pending() {
            self.futures.borrow_mut().put(task, future);
        }
        poll
    }

    /// Drops the futures of `tasks`, outside the borrow, since a drop may spawn.
    fn drop_futures(&self, tasks: &[u64]) {
        let futures = self.futures.borrow_mut().remove(tasks);
        drop(futures);
    }
}

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
        /// The panic message.
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
