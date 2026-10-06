//! The `env` drivers of a simulated node.

mod files;
mod net;
mod serial;

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use env::shards::{Config, Main};
use env::tasks::Task;
use env::thread::{Error, Handle};
use env::threads::Body;
use types::time::Monotonic;

use crate::shard::Fault;
use crate::state::{Due, Futures, Outcome, Shared, Start, State, lock};

/// Drives every seam of one node.
#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) shared: Shared,
    pub(crate) node: usize,
}

impl Node {
    /// Adds a thread that runs when the scheduler picks its first task.
    fn thread(&self, name: String, start: Start) -> Handle {
        let (thread, crashed) = lock(&self.shared).start(self.node, name, start);
        // After the lock: the drop may start a thread.
        drop(crashed);
        let (shared, node) = (Arc::clone(&self.shared), self.node);
        Handle::new(move || {
            let state = lock(&shared);
            let (outcome, name) = (state.outcome(thread), state.name(thread));
            drop(state);
            match outcome {
                Some(Outcome::Done(result)) => result,
                Some(Outcome::Crashed) => {
                    panic!("thread {name} ended in a crash of node {node}")
                }
                None => {
                    panic!("thread {name} has not ended; run the sim until it ends")
                }
            }
        })
    }

    /// The sim thread that runs now. `what` names the caller in a panic.
    ///
    /// # Panics
    ///
    /// Outside a thread that the sim started, and on a thread of another node.
    fn running(&self, what: &str) -> u64 {
        let current = lock(&self.shared).current();
        let Some((thread, on)) = current else {
            panic!("{what} needs a thread that the sim started")
        };
        let node = self.node;
        assert!(
            on == node,
            "{what} of node {node} runs on a thread of node {on}"
        );
        thread
    }
}

/// What a handle of a node binds to: the life of the node at its open, and the sim
/// thread of its first poll.
struct Owner {
    /// The handle, in a panic.
    what: &'static str,
    life: u64,
    thread: OnceLock<u64>,
}

impl Owner {
    /// The owner of a handle that `what` names in a panic, opened in `life` of its
    /// node.
    fn new(what: &'static str, life: u64) -> Self {
        Self {
            what,
            life,
            thread: OnceLock::new(),
        }
    }

    /// Whether `node` lives the life of the open in `state`: no crash of it since.
    fn current(&self, state: &State, node: usize) -> bool {
        state.life(node) == self.life
    }

    /// Binds the handle to the sim thread of `node` that polls it first.
    ///
    /// # Panics
    ///
    /// After a crash of `node`, as [`Node::running`], and on a thread other than the
    /// first.
    fn check(&self, node: &Node) {
        let (key, current) = (node.node, self.current(&lock(&node.shared), node.node));
        assert!(
            current,
            "{} of node {key} polls after a crash of the node",
            self.what
        );
        let thread = node.running(self.what);
        let first = *self.thread.get_or_init(|| thread);
        if first != thread {
            let name = lock(&node.shared).name(first);
            panic!(
                "{} polls only on thread {name:?} of its first poll",
                self.what
            );
        }
    }
}

impl env::clock::Driver for Node {
    fn now(&self) -> Monotonic {
        lock(&self.shared).monotonic(self.node)
    }

    fn epoch(&self) -> Instant {
        lock(&self.shared).epoch()
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        let thread = self.running("a sleep");
        let key = lock(&self.shared).key();
        Box::pin(Timer {
            shared: Arc::clone(&self.shared),
            node: self.node,
            thread,
            key,
            due: None,
        })
    }
}

impl env::wall::Driver for Node {
    fn now(&self) -> env::wall::Reading {
        lock(&self.shared).wall(self.node)
    }
}

impl env::entropy::Driver for Node {
    fn fill(&self, bytes: &mut [u8]) {
        lock(&self.shared).fill(self.node, bytes);
    }
}

impl env::shards::Driver for Node {
    fn cores(&self) -> NonZeroUsize {
        lock(&self.shared).cores(self.node)
    }

    fn pinnable(&self) -> bool {
        lock(&self.shared).pinnable(self.node)
    }

    fn start(&self, config: Config, main: Main) -> Result<Handle, Error> {
        let fault = lock(&self.shared).shards(self.node).record(&config);
        let name = config.name;
        let main = match fault {
            None => main,
            Some((_, Fault::Start)) => {
                let reason = "injected".into();
                return Err(Error::Start { name, reason });
            }
            Some((core, Fault::Pin)) => {
                let reason = "injected".into();
                return Err(Error::Pin { name, core, reason });
            }
            Some((_, Fault::Panic)) => Box::new(|tasks: env::tasks::Tasks| -> Task {
                tasks.spawn(async { panic!("injected") });
                let main = main(tasks);
                Box::pin(async move {
                    yield_now().await;
                    main.await;
                    panic!("injected");
                })
            }),
        };
        Ok(self.thread(name, Start::Shard(main)))
    }
}

/// Pending at its first poll, with its task woken, so the scheduler picks the next
/// task to run.
pub(crate) fn yield_now() -> impl Future<Output = ()> {
    let mut yielded = false;
    poll_fn(move |cx| {
        if mem::replace(&mut yielded, true) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
}

impl env::threads::Driver for Node {
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error> {
        Ok(self.thread(name.into(), Start::Body(body)))
    }
}

/// One timer: an entry in the run's timers while it waits.
struct Timer {
    shared: Shared,
    node: usize,
    /// The thread that made it, the only one that can poll it: a sleep is not `Send`.
    thread: u64,
    key: u64,
    /// The true deadline it asked to arm last, until it disarms.
    due: Option<Monotonic>,
}

impl env::clock::Timer for Timer {
    fn poll_until(
        self: Pin<&mut Self>,
        deadline: Monotonic,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        let this = self.get_mut();
        let waker = cx.waker().clone();
        let mut state = lock(&this.shared);
        let old = (this.due.take()).and_then(|at| state.disarm(at, this.key));
        let (poll, unused) = match state.due(this.node, deadline) {
            Due::Passed => (Poll::Ready(()), Some(waker)),
            Due::At(at) => {
                this.due = Some(at);
                (Poll::Pending, state.arm(this.thread, at, this.key, waker))
            }
            Due::Never => (Poll::Pending, Some(waker)),
        };
        drop(state);
        drop((old, unused));
        poll
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(at) = self.due {
            let waker = lock(&self.shared).disarm(at, self.key);
            drop(waker);
        }
    }
}

/// Spawns tasks on one simulated shard.
pub(crate) struct Tasks {
    pub(crate) shared: Shared,
    pub(crate) futures: Rc<RefCell<Futures>>,
    pub(crate) thread: u64,
}

impl env::tasks::Driver for Tasks {
    fn spawn(&self, task: Task) {
        let key = lock(&self.shared).spawn(self.thread);
        match key {
            Some(key) => self.futures.borrow_mut().insert(&self.shared, key, task),
            None => drop(task),
        }
    }
}
