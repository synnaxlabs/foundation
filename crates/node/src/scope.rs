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
            waker: Waker::noop().clone(),
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
        // Each slot drops before any future does, so a task that a future's drop polls
        // finds its slot gone and ends with no poll of its future. A future that drops
        // this scope in its own poll holds its slot, and its task ends when that poll
        // returns.
        let taken: Vec<Running> = running
            .into_values()
            .filter_map(|slot| Some(Rc::try_unwrap(slot).ok()?.into_inner()))
            .collect();
        // A future's drop may do anything, so it runs with no borrow held.
        let wakers: Vec<Waker> = taken
            .into_iter()
            .map(|Running { future, waker }| {
                drop(future);
                waker
            })
            .collect();
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
    waker: Waker,
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
            running.waker.clone_from(cx.waker());
            running.future.as_mut().poll(cx)
        };
        if self.running.strong_count() == 0 {
            // The future dropped the scope in this poll, so this holds its last
            // reference.
            drop(slot);
            return Poll::Ready(());
        }
        if polled.is_ready() {
            drop(slot);
            let running = self.running.upgrade().expect("invariant: a live scope");
            let done = running.borrow_mut().remove(&self.key);
            // A future's drop may do anything, so it runs with no borrow held, and with
            // no reference to the map, so that a task it polls sees its scope drop.
            drop(running);
            drop(done);
        }
        polled
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::cell::{Cell, RefCell};
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
        scope.spawn(Box::pin(poll_fn(move |_| {
            let _ = &inner;
            Poll::Ready(())
        })));
        let mut task = take(&queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(Rc::strong_count(&held), 1);
        drop(scope);
    }

    #[test]
    fn a_dropped_scope_wakes_the_waker_of_the_last_poll() {
        let (mut scope, queued) = scope();
        scope.spawn(Box::pin(pending()));
        let mut task = take(&queued);
        let (first, first_woken) = waker();
        assert_eq!(
            task.as_mut().poll(&mut Context::from_waker(&first)),
            Poll::Pending
        );
        let (last, last_woken) = waker();
        assert_eq!(
            task.as_mut().poll(&mut Context::from_waker(&last)),
            Poll::Pending
        );
        drop(scope);
        assert_eq!(first_woken.0.load(Ordering::Relaxed), 0);
        assert_eq!(last_woken.0.load(Ordering::Relaxed), 1);
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

    /// Drops the scope in its `Option` when it drops.
    struct DropsScope(Rc<RefCell<Option<Scope>>>);

    impl Drop for DropsScope {
        fn drop(&mut self) {
            drop(self.0.borrow_mut().take());
        }
    }

    /// Spawns on `scope` a future that completes at its first poll and whose drop
    /// drops the scope, then polls its task, which must end. Gives the scope's
    /// `Option`.
    fn drop_at_completion(
        scope: Scope,
        queued: &RefCell<Vec<Task>>,
    ) -> Rc<RefCell<Option<Scope>>> {
        let owner = Rc::new(RefCell::new(Some(scope)));
        let guard = DropsScope(Rc::clone(&owner));
        let future: Task = Box::pin(poll_fn(move |_| {
            let _ = &guard;
            Poll::Ready(())
        }));
        owner.borrow_mut().as_mut().unwrap().spawn(future);
        let mut task = take(queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        owner
    }

    #[test]
    fn a_future_whose_drop_at_completion_drops_its_scope_ends_its_task() {
        let (scope, queued) = scope();
        let owner = drop_at_completion(scope, &queued);
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

    /// What each poll of a task gave.
    type Polled = Rc<RefCell<Vec<Poll<()>>>>;

    /// Polls the task in `task`, if any, when it drops, and keeps what the poll gave.
    struct PollsOnDrop {
        task: Rc<RefCell<Option<Task>>>,
        polled: Polled,
    }

    impl Drop for PollsOnDrop {
        fn drop(&mut self) {
            if let Some(task) = self.task.borrow_mut().as_mut() {
                let (waker, _) = waker();
                let polled = task.as_mut().poll(&mut Context::from_waker(&waker));
                self.polled.borrow_mut().push(polled);
            }
        }
    }

    /// Spawns on `scope` 16 futures that each poll the task in `other` when they drop,
    /// so that some drop before its slot, then a pending future that counts its polls,
    /// whose task polls once and goes into `other`. Gives what each poll of the task
    /// gave, and the count.
    fn create_polls_on_drop(
        scope: &mut Scope,
        queued: &RefCell<Vec<Task>>,
        other: &Rc<RefCell<Option<Task>>>,
    ) -> (Polled, Rc<Cell<u32>>) {
        let polled = Rc::default();
        for _ in 0..16 {
            let guard = PollsOnDrop {
                task: Rc::clone(other),
                polled: Rc::clone(&polled),
            };
            scope.spawn(Box::pin(async move {
                pending::<()>().await;
                drop(guard);
            }));
            drop(take(queued));
        }
        let runs = Rc::new(Cell::new(0));
        let counted = Rc::clone(&runs);
        scope.spawn(Box::pin(poll_fn(move |_| {
            counted.set(counted.get() + 1);
            Poll::<()>::Pending
        })));
        let mut task = take(queued);
        let (waker, _) = waker();
        assert_eq!(
            task.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        *other.borrow_mut() = Some(task);
        (polled, runs)
    }

    #[test]
    fn a_task_polled_in_a_drop_while_its_scope_drops_ends() {
        let (mut scope, queued) = scope();
        let other = Rc::default();
        let (polled, runs) = create_polls_on_drop(&mut scope, &queued, &other);
        drop(scope);
        assert_eq!(*polled.borrow(), vec![Poll::Ready(()); 16]);
        assert_eq!(runs.get(), 1, "a future runs no more once its scope drops");
    }

    #[test]
    fn a_task_polled_in_a_drop_while_a_completed_future_drops_its_scope_ends() {
        let (mut scope, queued) = scope();
        let other = Rc::default();
        let (polled, runs) = create_polls_on_drop(&mut scope, &queued, &other);
        drop_at_completion(scope, &queued);
        assert_eq!(*polled.borrow(), vec![Poll::Ready(()); 16]);
        assert_eq!(runs.get(), 1, "a future runs no more once its scope drops");
    }

    #[test]
    fn a_task_that_drops_its_scope_in_a_drop_at_completion_ends() {
        let (scope, queued) = scope();
        let owner = Rc::new(RefCell::new(Some(scope)));
        let other: Rc<RefCell<Option<Task>>> = Rc::default();
        let spawn = |future| owner.borrow_mut().as_mut().unwrap().spawn(future);
        spawn(drop_scope(&owner, Poll::Pending));
        *other.borrow_mut() = Some(take(&queued));
        let polled: Polled = Rc::default();
        let guard = PollsOnDrop {
            task: Rc::clone(&other),
            polled: Rc::clone(&polled),
        };
        spawn(Box::pin(poll_fn(move |_| {
            let _ = &guard;
            Poll::Ready(())
        })));
        let mut task = take(&queued);
        let (waker, _) = waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(()));
        assert!(owner.borrow().is_none());
        assert_eq!(*polled.borrow(), vec![Poll::Ready(())]);
    }
}
