//! The `env` drivers of a simulated node.

mod net;

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use env::shards::{Config, Main};
use env::tasks::Task;
use env::thread::{Error, Handle};
use env::threads::Body;
use types::time::{Monotonic, Stamp};

use crate::state::{Due, Futures, Shared, Start, lock};

/// Drives every seam of one node.
#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) shared: Shared,
    pub(crate) node: usize,
}

impl Node {
    /// Adds a thread that runs when the scheduler picks its first task.
    fn thread(&self, name: String, start: Start) -> Handle {
        let thread = lock(&self.shared).start(self.node, name, start);
        let shared = Arc::clone(&self.shared);
        Handle::new(move || {
            let state = lock(&shared);
            let (outcome, name) = (state.outcome(thread), state.name(thread));
            drop(state);
            outcome.unwrap_or_else(|| {
                panic!("thread {name} has not ended; run the sim until it ends")
            })
        })
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
        let mut state = lock(&self.shared);
        let on = state.current().map(|(_, node)| node);
        let key = state.key();
        drop(state);
        let Some(on) = on else {
            panic!("a sleep needs a thread that the sim started")
        };
        assert!(
            on == self.node,
            "a clock of node {} sleeps on a thread of node {on}",
            self.node
        );
        Box::pin(Timer {
            shared: Arc::clone(&self.shared),
            node: self.node,
            key,
            due: None,
        })
    }
}

impl env::wall::Driver for Node {
    fn now(&self) -> Stamp {
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

    fn start(&self, config: Config, main: Main) -> Result<Handle, Error> {
        if let Some(core) = config.core.filter(|&core| core >= self.cores().get()) {
            let name = config.name;
            return Err(Error::Pin { name, core });
        }
        Ok(self.thread(config.name, Start::Shard(main)))
    }
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
    key: u64,
    /// The true deadline of its entry, while it has one.
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
                state.arm(at, this.key, waker);
                this.due = Some(at);
                (Poll::Pending, None)
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
