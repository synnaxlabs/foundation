//! Futures that run on `env::tasks` and drop together.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};

use types::hash;

/// A boxed future that a [`Scope`] runs.
pub(crate) type Boxed = Pin<Box<dyn Future<Output = ()>>>;

/// Runs futures on `env::tasks`. Dropped, it drops each future that has not completed.
/// A future that completes drops at once.
pub(crate) struct Scope {
    running: Rc<RefCell<hash::Map<u64, Slot>>>,
    tasks: env::tasks::Tasks,
    next: u64,
}

impl Scope {
    /// An empty scope whose futures run on `tasks`.
    pub(crate) fn new(tasks: env::tasks::Tasks) -> Self {
        Self {
            running: Rc::default(),
            tasks,
            next: 0,
        }
    }

    /// Runs `future` on the scope's tasks until it completes or the scope drops.
    pub(crate) fn spawn(&mut self, future: Boxed) {
        let slot = Rc::new(RefCell::new(future));
        let run = Spawned {
            slot: Rc::downgrade(&slot),
            running: Rc::downgrade(&self.running),
            number: self.next,
        };
        self.running.borrow_mut().insert(self.next, slot);
        self.next += 1;
        self.tasks.spawn(poll_fn(move |cx| run.poll(cx)));
    }
}

/// One running future. Only the scope's map holds it, so a drop of the scope drops it.
type Slot = Rc<RefCell<Boxed>>;

/// What the executor holds of one running future.
struct Spawned {
    slot: Weak<RefCell<Boxed>>,
    running: Weak<RefCell<hash::Map<u64, Slot>>>,
    /// The future's key in `running`.
    number: u64,
}

impl Spawned {
    /// Polls the future, then drops it from `running` once it completes. Ends once the
    /// future has dropped.
    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(slot) = self.slot.upgrade() else {
            return Poll::Ready(());
        };
        let polled = slot.borrow_mut().as_mut().poll(cx);
        if polled.is_ready() {
            drop(slot);
            let running = self.running.upgrade();
            let running = running.expect("invariant: a live slot is in the live map");
            let done = running.borrow_mut().remove(&self.number);
            // A future's drop may do anything, so it runs with no borrow held.
            drop(done);
        }
        polled
    }
}
