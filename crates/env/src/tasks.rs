//! Spawning tasks on the current shard.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// A task as a driver receives it.
///
/// ```
/// let task: env::tasks::Task = Box::pin(async {});
/// ```
pub type Task = Pin<Box<dyn Future<Output = ()>>>;

/// Spawns tasks on one shard. Its tasks run only on that shard's thread, so they need
/// not be `Send`, and the handle cannot leave the thread. Clones spawn on the same
/// shard.
///
/// A shard's main function receives it from
/// [`Shards::start`](crate::shards::Shards::start).
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
///
/// fn count(tasks: &env::tasks::Tasks) -> Rc<Cell<u32>> {
///     let n = Rc::new(Cell::new(0));
///     let shared = Rc::clone(&n);
///     tasks.spawn(async move { shared.set(shared.get() + 1) });
///     n
/// }
/// ```
#[derive(Clone)]
pub struct Tasks(Rc<dyn Driver>);

impl Tasks {
    /// Wraps `driver`.
    ///
    /// ```
    /// fn wrap(driver: impl env::tasks::Driver + 'static) -> env::tasks::Tasks {
    ///     env::tasks::Tasks::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Rc::new(driver))
    }

    /// Starts `task` on this shard. It has no handle: stop it through its own cancel
    /// token, and send results back through a channel.
    ///
    /// It runs until its future completes or the shard's main future completes,
    /// which drops it. A panic in it ends the shard (see
    /// [`Shards::start`](crate::shards::Shards::start)).
    ///
    /// ```
    /// fn start(tasks: &env::tasks::Tasks) {
    ///     tasks.spawn(async {});
    /// }
    /// ```
    pub fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        self.0.spawn(Box::pin(task));
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish_non_exhaustive()
    }
}

/// What [`Tasks`] runs its tasks on. `os` and `sim` implement it for a shard. A driver
/// that counts tasks passes each task to one of theirs. A driver of a test or a
/// benchmark may keep each task for its caller to poll.
///
/// ```
/// fn wrap(driver: impl env::tasks::Driver + 'static) -> env::tasks::Tasks {
///     env::tasks::Tasks::new(driver)
/// }
/// ```
pub trait Driver {
    /// Starts `task` on this driver's shard, with the rules of [`Tasks::spawn`].
    fn spawn(&self, task: Task);
}

/// Counts each task spawned through it until the task's future has dropped. Clones
/// share the count, and spawn on the shard of the [`Tasks`] that made it. Wrap a
/// clone with [`Tasks::new`] to spawn through it.
///
/// ```
/// use env::tasks::{Group, Tasks};
///
/// async fn run(tasks: Tasks) {
///     let group = Group::new(tasks);
///     Tasks::new(group.clone()).spawn(async {});
///     group.ended().await;
/// }
/// ```
#[derive(Clone)]
pub struct Group {
    tasks: Tasks,
    count: Rc<Count>,
}

impl Group {
    /// A group with no task, which spawns on `tasks`.
    #[must_use]
    pub fn new(tasks: Tasks) -> Self {
        Self {
            tasks,
            count: Rc::default(),
        }
    }

    /// A future that is ready when it is polled while the group has no task: each
    /// task spawned through it completed, or its shard dropped it, and the task's
    /// future has dropped. It holds no clone of the group or of its [`Tasks`], and
    /// each call gives its own.
    #[must_use]
    pub fn ended(&self) -> Ended {
        let slot = self.count.slots.get();
        self.count.slots.set(slot.strict_add(1));
        Ended {
            count: Rc::clone(&self.count),
            slot,
        }
    }
}

impl fmt::Debug for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Group").finish_non_exhaustive()
    }
}

impl Driver for Group {
    fn spawn(&self, task: Task) {
        self.count.live.set(self.count.live.get().strict_add(1));
        self.tasks.spawn(Counted {
            task,
            _held: Held(Rc::clone(&self.count)),
        });
    }
}

// The fields drop in order, so the task's future drops before its count, also when
// the shard drops it while it waits.
struct Counted {
    task: Task,
    _held: Held,
}

impl Future for Counted {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.task.as_mut().poll(cx)
    }
}

struct Held(Rc<Count>);

impl Drop for Held {
    fn drop(&mut self) {
        let live = self.0.live.get().strict_sub(1);
        self.0.live.set(live);
        if live == 0 {
            self.0.waiting.take().into_values().for_each(Waker::wake);
        }
    }
}

#[derive(Default)]
struct Count {
    live: Cell<usize>,
    // The waker of each `Ended` that waits, by its slot.
    waiting: RefCell<BTreeMap<u64, Waker>>,
    // The slot of the next `Ended`.
    slots: Cell<u64>,
}

/// Ready once no task of a [`Group`] is left. [`Group::ended`] gives it. Its drop
/// removes its waker.
pub struct Ended {
    count: Rc<Count>,
    slot: u64,
}

impl fmt::Debug for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ended").finish_non_exhaustive()
    }
}

impl Future for Ended {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.count.live.get() == 0 {
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        // The old waker drops after the borrow: its drop can drop a task or an
        // `Ended` of this group.
        let replaced = self.count.waiting.borrow_mut().insert(self.slot, waker);
        drop(replaced);
        Poll::Pending
    }
}

impl Drop for Ended {
    fn drop(&mut self) {
        let removed = self.count.waiting.borrow_mut().remove(&self.slot);
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Wake;

    use proptest::prelude::*;

    use super::*;

    /// A driver that keeps each task for the test to poll or drop, as a shard would.
    #[derive(Clone, Default)]
    struct Kept(Rc<RefCell<Vec<Option<Task>>>>);

    impl Driver for Kept {
        fn spawn(&self, task: Task) {
            self.0.borrow_mut().push(Some(task));
        }
    }

    impl Kept {
        /// Polls task `i` once, and drops it when it completes.
        fn poll(&self, i: usize) {
            let mut kept = self.0.borrow_mut();
            let slot = kept.get_mut(i).expect("a task at i");
            let task = slot.as_mut().expect("a live task");
            let mut cx = Context::from_waker(Waker::noop());
            if task.as_mut().poll(&mut cx).is_ready() {
                let done = slot.take();
                drop(kept);
                drop(done);
            }
        }

        /// Drops task `i` while it waits, as a shard does when its main future ends.
        fn drop_task(&self, i: usize) {
            let task = self.0.borrow_mut().get_mut(i).and_then(Option::take);
            drop(task.expect("a live task"));
        }
    }

    /// A waker that records whether it was woken.
    #[derive(Default)]
    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    impl Flag {
        fn woken(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn poll(ended: &mut Ended, waker: &Waker) -> Poll<()> {
        Pin::new(ended).poll(&mut Context::from_waker(waker))
    }

    fn noop(ended: &mut Ended) -> Poll<()> {
        poll(ended, Waker::noop())
    }

    /// A task that completes once `done` is set.
    fn until(done: &Rc<Cell<bool>>) -> impl Future<Output = ()> + use<> {
        let done = Rc::clone(done);
        std::future::poll_fn(move |_| {
            if done.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn group() -> (Kept, Group) {
        let kept = Kept::default();
        (kept.clone(), Group::new(Tasks::new(kept)))
    }

    #[test]
    fn ended_is_ready_at_once_for_a_group_with_no_task() {
        let (_, group) = group();
        assert_eq!(noop(&mut group.ended()), Poll::Ready(()));
    }

    #[test]
    fn a_task_spawned_through_the_group_runs_on_its_driver() {
        let (kept, group) = group();
        let ran = Rc::new(Cell::new(false));
        let mark = Rc::clone(&ran);
        Tasks::new(group).spawn(async move { mark.set(true) });
        kept.poll(0);
        assert!(ran.get());
    }

    /// Records, at its drop, whether a wait of its group was still pending.
    struct Probe {
        ended: Ended,
        pending: Rc<Cell<Option<bool>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.pending.set(Some(noop(&mut self.ended).is_pending()));
        }
    }

    fn probed(group: &Group, done: &Rc<Cell<bool>>) -> Rc<Cell<Option<bool>>> {
        let pending = Rc::new(Cell::new(None));
        let probe = Probe {
            ended: group.ended(),
            pending: Rc::clone(&pending),
        };
        let done = Rc::clone(done);
        // A `poll_fn` keeps the probe until the future drops, not only until it
        // completes.
        Tasks::new(group.clone()).spawn(std::future::poll_fn(move |_| {
            let _ = &probe;
            if done.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        pending
    }

    #[test]
    fn ended_waits_for_the_drop_of_a_task_that_completes() {
        let (kept, group) = group();
        let done = Rc::new(Cell::new(false));
        let pending = probed(&group, &done);
        let mut ended = group.ended();
        kept.poll(0);
        assert_eq!(noop(&mut ended), Poll::Pending);
        done.set(true);
        kept.poll(0);
        assert_eq!(pending.get(), Some(true));
        assert_eq!(noop(&mut ended), Poll::Ready(()));
    }

    #[test]
    fn ended_waits_for_the_drop_of_a_task_its_driver_drops() {
        let (kept, group) = group();
        let pending = probed(&group, &Rc::new(Cell::new(false)));
        let mut ended = group.ended();
        kept.poll(0);
        kept.drop_task(0);
        assert_eq!(pending.get(), Some(true));
        assert_eq!(noop(&mut ended), Poll::Ready(()));
    }

    #[test]
    fn each_waiting_ended_is_woken() {
        let (kept, group) = group();
        let done = Rc::new(Cell::new(false));
        Tasks::new(group.clone()).spawn(until(&done));
        let flags = [Arc::new(Flag::default()), Arc::new(Flag::default())];
        let mut waits = [group.ended(), group.ended()];
        for (ended, flag) in waits.iter_mut().zip(&flags) {
            assert_eq!(poll(ended, &Waker::from(Arc::clone(flag))), Poll::Pending);
        }
        done.set(true);
        kept.poll(0);
        assert!(flags.iter().all(|flag| flag.woken()));
        for ended in &mut waits {
            assert_eq!(noop(ended), Poll::Ready(()));
        }
    }

    #[test]
    fn a_dropped_ended_holds_no_waker() {
        let (_kept, group) = group();
        Tasks::new(group.clone()).spawn(std::future::pending());
        let flag = Arc::new(Flag::default());
        let mut ended = group.ended();
        assert_eq!(
            poll(&mut ended, &Waker::from(Arc::clone(&flag))),
            Poll::Pending
        );
        drop(ended);
        assert_eq!(Arc::strong_count(&flag), 1);
    }

    #[test]
    fn an_ended_polled_twice_holds_one_waker() {
        let (_kept, group) = group();
        Tasks::new(group.clone()).spawn(std::future::pending());
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(Arc::clone(&flag));
        let mut ended = group.ended();
        for _ in 0..2 {
            assert_eq!(poll(&mut ended, &waker), Poll::Pending);
        }
        drop(waker);
        assert_eq!(Arc::strong_count(&flag), 2);
    }

    #[test]
    fn ended_holds_no_clone_of_the_group_or_its_tasks() {
        let (kept, group) = group();
        let ended = group.ended();
        drop(group);
        assert_eq!(Rc::strong_count(&kept.0), 1);
        drop(ended);
    }

    #[test]
    fn the_debug_text_of_a_group_and_of_ended_holds_nothing() {
        let (_, group) = group();
        assert_eq!(format!("{group:?}"), "Group { .. }");
        assert_eq!(format!("{:?}", group.ended()), "Ended { .. }");
    }

    #[derive(Clone, Debug)]
    enum Step {
        Spawn,
        Complete(usize),
        Drop(usize),
        Wait,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            Just(Step::Spawn),
            any::<usize>().prop_map(Step::Complete),
            any::<usize>().prop_map(Step::Drop),
            Just(Step::Wait),
        ]
    }

    proptest! {
        #[test]
        fn ended_is_ready_exactly_when_no_task_is_left(
            steps in proptest::collection::vec(step(), 0..64),
        ) {
            let (kept, group) = group();
            let mut flags = Vec::new();
            let mut live: Vec<(usize, Rc<Cell<bool>>)> = Vec::new();
            for step in steps {
                match step {
                    Step::Spawn => {
                        let done = Rc::new(Cell::new(false));
                        Tasks::new(group.clone()).spawn(until(&done));
                        live.push((kept.0.borrow().len() - 1, done));
                    }
                    Step::Complete(i) if !live.is_empty() => {
                        let (task, done) = live.remove(i % live.len());
                        done.set(true);
                        kept.poll(task);
                    }
                    Step::Drop(i) if !live.is_empty() => {
                        let (task, _) = live.remove(i % live.len());
                        kept.drop_task(task);
                    }
                    Step::Wait => {
                        let flag = Arc::new(Flag::default());
                        let mut ended = group.ended();
                        let polled = poll(&mut ended, &Waker::from(Arc::clone(&flag)));
                        flags.push((ended, flag, polled.is_pending()));
                        prop_assert_eq!(polled.is_ready(), live.is_empty());
                    }
                    Step::Complete(_) | Step::Drop(_) => {}
                }
                if live.is_empty() {
                    for (mut ended, flag, waited) in flags.drain(..) {
                        prop_assert!(!waited || flag.woken());
                        prop_assert_eq!(noop(&mut ended), Poll::Ready(()));
                    }
                }
                prop_assert_eq!(noop(&mut group.ended()).is_ready(), live.is_empty());
            }
        }
    }

    #[test]
    fn a_task_whose_drop_spawns_keeps_the_group_live() {
        struct Spawns(Group);
        impl Drop for Spawns {
            fn drop(&mut self) {
                Tasks::new(self.0.clone()).spawn(std::future::pending());
            }
        }
        let (kept, group) = group();
        let spawns = Spawns(group.clone());
        Tasks::new(group.clone()).spawn(async move {
            let _spawns = spawns;
            std::future::pending::<()>().await;
        });
        let flag = Arc::new(Flag::default());
        let mut ended = group.ended();
        assert_eq!(
            poll(&mut ended, &Waker::from(Arc::clone(&flag))),
            Poll::Pending
        );
        kept.poll(0);
        kept.drop_task(0);
        assert!(!flag.woken());
        assert_eq!(
            poll(&mut ended, &Waker::from(Arc::clone(&flag))),
            Poll::Pending
        );
        kept.drop_task(1);
        assert!(flag.woken());
        assert_eq!(noop(&mut ended), Poll::Ready(()));
    }
}
