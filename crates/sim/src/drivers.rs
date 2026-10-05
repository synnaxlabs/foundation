//! The `env` drivers of a simulated node.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use env::shards::{Config, Main};
use env::tasks::Task;
use env::threads::{Body, Error, Handle};
use types::time::{Monotonic, Stamp};

use crate::state::{Futures, Shared, Start, lock};

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
            let thread = &state.threads[&thread];
            if let Some(outcome) = thread.outcome.clone() {
                return outcome;
            }
            let name = thread.name.clone();
            drop(state);
            panic!("thread {name} has not ended; run the sim until it ends")
        })
    }
}

/// Rejects a name that the OS would reject.
fn named(name: &str) -> Result<(), Error> {
    if name.contains('\0') {
        return Err(Error::Start {
            name: name.into(),
            reason: "the name holds a NUL byte".into(),
        });
    }
    Ok(())
}

impl env::clock::Driver for Node {
    fn now(&self) -> Monotonic {
        lock(&self.shared).monotonic(self.node)
    }

    fn epoch(&self) -> Instant {
        lock(&self.shared).epoch
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        let mut state = lock(&self.shared);
        let on = (state.current)
            .filter(|_| thread::current().id() == state.runner)
            .map(|thread| state.threads[&thread].node);
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
        let state = lock(&self.shared);
        state.nodes[self.node].wall + state.elapsed()
    }
}

impl env::entropy::Driver for Node {
    fn fill(&self, bytes: &mut [u8]) {
        lock(&self.shared).nodes[self.node].entropy.fill(bytes);
    }
}

impl env::shards::Driver for Node {
    fn cores(&self) -> NonZeroUsize {
        lock(&self.shared).nodes[self.node].cores
    }

    fn start(&self, config: Config, main: Main) -> Result<Handle, Error> {
        named(&config.name)?;
        if let Some(core) = config.core.filter(|&core| core >= self.cores().get()) {
            let name = config.name;
            return Err(Error::Pin { name, core });
        }
        Ok(self.thread(config.name, Start::Shard(main)))
    }
}

impl env::threads::Driver for Node {
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error> {
        named(name)?;
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
        let old =
            (this.due.take()).and_then(|due| state.timers.remove(&(due, this.key)));
        let now = state.monotonic(this.node);
        let poll = if deadline <= now {
            Poll::Ready(())
        } else {
            let due = state.now + (deadline - now);
            state.timers.insert((due, this.key), waker);
            this.due = Some(due);
            Poll::Pending
        };
        drop(state);
        drop(old);
        poll
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(due) = self.due {
            let waker = lock(&self.shared).timers.remove(&(due, self.key));
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
