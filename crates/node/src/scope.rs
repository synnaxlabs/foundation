//! Futures that run on `env::tasks` and drop together.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use env::tasks::Task;
use types::hash;

/// Runs futures on `env::tasks`. Dropped, it drops each future that has not completed
/// and wakes its task, which then ends. A future that completes drops at once.
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
    pub(crate) fn spawn(&mut self, future: Task) {
        let slot = Rc::new(RefCell::new(Running {
            future,
            waker: None,
        }));
        let run = Spawned {
            slot: Rc::downgrade(&slot),
            running: Rc::downgrade(&self.running),
            key: self.next,
        };
        self.running.borrow_mut().insert(self.next, slot);
        self.next += 1;
        self.tasks.spawn(poll_fn(move |cx| run.poll(cx)));
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let running = mem::take(&mut *self.running.borrow_mut());
        // The slot of a future that drops this scope in its own poll is borrowed. Its
        // task ends when that poll returns.
        let wakers: Vec<Waker> = running
            .values()
            .filter_map(|slot| slot.try_borrow_mut().ok()?.waker.take())
            .collect();
        // A future's drop may do anything, so it runs with no borrow held.
        drop(running);
        for waker in wakers {
            waker.wake();
        }
    }
}

/// One running future. Only the scope's map holds it, so a drop of the scope drops it.
type Slot = Rc<RefCell<Running>>;

struct Running {
    future: Task,
    /// The waker of the future's task at its last poll.
    waker: Option<Waker>,
}

/// What the executor holds of one running future.
struct Spawned {
    slot: Weak<RefCell<Running>>,
    running: Weak<RefCell<hash::Map<u64, Slot>>>,
    key: u64,
}

impl Spawned {
    /// Polls the future, then drops it from `running` once it completes. Ends once the
    /// future has dropped.
    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(slot) = self.slot.upgrade() else {
            return Poll::Ready(());
        };
        let polled = {
            let mut running = slot.borrow_mut();
            match &running.waker {
                Some(waker) if waker.will_wake(cx.waker()) => {}
                _ => running.waker = Some(cx.waker().clone()),
            }
            running.future.as_mut().poll(cx)
        };
        let Some(running) = self.running.upgrade() else {
            // The future dropped the scope in this poll, so this holds its last
            // reference.
            drop(slot);
            return Poll::Ready(());
        };
        if polled.is_ready() {
            drop(slot);
            let done = running.borrow_mut().remove(&self.key);
            drop(done);
        }
        polled
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::cell::RefCell;
    use std::future::{pending, poll_fn};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use env::tasks::Task;

    use super::Scope;

    /// Keeps each task for the test to poll.
    struct Queued(Rc<RefCell<Vec<Task>>>);

    impl env::tasks::Driver for Queued {
        fn spawn(&self, task: Task) {
            self.0.borrow_mut().push(task);
        }
    }

    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn scope() -> (Scope, Rc<RefCell<Vec<Task>>>) {
        let queued = Rc::default();
        let tasks = env::tasks::Tasks::new(Queued(Rc::clone(&queued)));
        (Scope::new(tasks), queued)
    }

    fn waker() -> (Waker, Arc<Wakes>) {
        let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
        (Waker::from(Arc::clone(&wakes)), wakes)
    }

    fn take(queued: &RefCell<Vec<Task>>) -> Task {
        queued.borrow_mut().pop().expect("a task was spawned")
    }

    #[test]
    fn a_dropped_scope_drops_a_pending_future_and_ends_its_task() {
        let (mut scope, queued) = scope();
        let held = Rc::new(());
        let inner = Rc::clone(&held);
        scope.spawn(Box::pin(async move {
            pending::<()>().await;
            drop(inner);
        }));
        let mut task = take(&queued);
        let (waker, woken) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Pending);
        drop(scope);
        assert_eq!(Rc::strong_count(&held), 1);
        assert_eq!(woken.0.load(Ordering::Relaxed), 1);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn a_future_that_completes_drops_at_once() {
        let (mut scope, queued) = scope();
        let held = Rc::new(());
        let inner = Rc::clone(&held);
        scope.spawn(Box::pin(async move { drop(inner) }));
        let mut task = take(&queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(Rc::strong_count(&held), 1);
        drop(scope);
    }

    /// Drops the scope in `owner` in its first poll, then gives `then`.
    fn drop_scope(owner: &Rc<RefCell<Option<Scope>>>, then: Poll<()>) -> Task {
        let owner = Rc::clone(owner);
        Box::pin(poll_fn(move |_| {
            drop(owner.borrow_mut().take());
            then
        }))
    }

    #[test]
    fn a_future_that_drops_its_scope_and_completes_ends_its_task() {
        let (scope, queued) = scope();
        let owner = Rc::new(RefCell::new(Some(scope)));
        let future = drop_scope(&owner, Poll::Ready(()));
        owner.borrow_mut().as_mut().unwrap().spawn(future);
        let mut task = take(&queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(owner.borrow().is_none());
    }

    #[test]
    fn a_future_that_drops_its_scope_and_waits_ends_its_task_and_drops() {
        let (scope, queued) = scope();
        let owner = Rc::new(RefCell::new(Some(scope)));
        let future = drop_scope(&owner, Poll::Pending);
        owner.borrow_mut().as_mut().unwrap().spawn(future);
        let mut task = take(&queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        // The future and its clone of `owner` dropped.
        assert_eq!(Rc::strong_count(&owner), 1);
    }
}
