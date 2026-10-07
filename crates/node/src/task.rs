//! Tasks given from any thread that run with shard 0's hub, in the order given.

use std::cell::RefCell;
use std::fmt;
use std::mem;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

#[cfg(loom)]
use loom::sync::{Arc, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Arc, Mutex, MutexGuard};

use hub::Hub;
use types::hash;

/// A task of [`crate::Node::spawn`], with its future boxed.
pub(crate) type Task = Box<dyn FnOnce(Hub) -> Boxed + Send>;

/// The future of a [`Task`].
pub(crate) type Boxed = Pin<Box<dyn Future<Output = ()>>>;

/// The two ends of the queue of tasks for shard 0. A task is pushed once, never on a
/// frame's path, so a mutex is fine.
pub(crate) fn pair<T>() -> (Queue<T>, Inbox<T>) {
    let state = Arc::new(Mutex::new(State {
        tasks: Vec::new(),
        waker: None,
        closed: false,
    }));
    (Queue(Arc::clone(&state)), Inbox(state))
}

struct State<T> {
    tasks: Vec<T>,
    waker: Option<Waker>,
    /// Whether the inbox has dropped.
    closed: bool,
}

fn lock<T>(state: &Mutex<State<T>>) -> MutexGuard<'_, State<T>> {
    state
        .lock()
        .expect("invariant: nothing panics while it holds a task queue lock")
}

/// The end that gives tasks, from any thread.
pub(crate) struct Queue<T>(Arc<Mutex<State<T>>>);

impl<T> Queue<T> {
    /// Gives `task` to the [`Inbox`], or drops it once the inbox has dropped.
    pub(crate) fn push(&self, task: T) {
        let mut state = lock(&self.0);
        if state.closed {
            // A task's drop may do anything, so it runs with no lock held.
            drop(state);
            drop(task);
            return;
        }
        state.tasks.push(task);
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> fmt::Debug for Queue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Queue")
    }
}

/// The end that takes tasks, on shard 0. Dropped, it drops each task it holds and
/// each task given after.
pub(crate) struct Inbox<T>(Arc<Mutex<State<T>>>);

impl<T> Inbox<T> {
    /// Gives each task pushed since the last call, oldest first, else wakes `cx` at
    /// the next push.
    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<Vec<T>> {
        let mut state = lock(&self.0);
        if state.tasks.is_empty() {
            state.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(mem::take(&mut state.tasks))
    }
}

impl<T> Drop for Inbox<T> {
    fn drop(&mut self) {
        let tasks = {
            let mut state = lock(&self.0);
            state.closed = true;
            mem::take(&mut state.tasks)
        };
        drop(tasks);
    }
}

/// Runs tasks with one hub. Each task starts at its first poll, once each task given
/// before it has started. Dropping it drops the hub and each task, run or not.
pub(crate) struct Runner(Rc<RefCell<Set>>);

struct Set {
    hub: Hub,
    tasks: env::tasks::Tasks,
    /// The number of the next task given.
    given: u64,
    /// The number of the next task to start.
    turn: u64,
    /// The tasks that wait to start, by number.
    queued: hash::Map<u64, Task>,
    /// The waker of each queued task that was polled before its turn.
    waiting: hash::Map<u64, Waker>,
    running: hash::Map<u64, Boxed>,
}

impl Runner {
    /// A runner of tasks with `hub`, which spawns them on `tasks`.
    pub(crate) fn new(hub: Hub, tasks: env::tasks::Tasks) -> Self {
        Self(Rc::new(RefCell::new(Set {
            hub,
            tasks,
            given: 0,
            turn: 0,
            queued: hash::Map::default(),
            waiting: hash::Map::default(),
            running: hash::Map::default(),
        })))
    }

    /// Spawns `task`, which starts after each task given before it.
    pub(crate) fn spawn(&self, task: Task) {
        let mut set = self.0.borrow_mut();
        let number = set.given;
        set.given += 1;
        set.queued.insert(number, task);
        let weak = Rc::downgrade(&self.0);
        set.tasks
            .spawn(std::future::poll_fn(move |cx| poll(&weak, number, cx)));
    }
}

/// Polls task `number` of `set`, which ends once the runner has dropped.
fn poll(set: &Weak<RefCell<Set>>, number: u64, cx: &mut Context<'_>) -> Poll<()> {
    let Some(set) = set.upgrade() else {
        return Poll::Ready(());
    };
    let mut set = set.borrow_mut();
    let set = &mut *set;
    if let Some(task) = set.queued.remove(&number) {
        if number > set.turn {
            set.queued.insert(number, task);
            set.waiting.insert(number, cx.waker().clone());
            return Poll::Pending;
        }
        set.turn += 1;
        if let Some(next) = set.waiting.remove(&set.turn) {
            next.wake();
        }
        set.running.insert(number, task(set.hub.clone()));
    }
    let future = set
        .running
        .get_mut(&number)
        .expect("invariant: a task that has not ended is queued or running");
    let polled = future.as_mut().poll(cx);
    if polled.is_ready() {
        set.running.remove(&number);
    }
    polled
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::sync::Arc;

    use super::pair;

    #[test]
    fn a_task_pushed_after_the_inbox_drops_is_dropped() {
        let (queue, inbox) = pair();
        assert_eq!(format!("{queue:?}"), "Queue");
        drop(inbox);
        let task = Arc::new(());
        queue.push(Arc::clone(&task));
        assert_eq!(Arc::strong_count(&task), 1);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use loom::future::block_on;
    use loom::thread;

    use super::pair;

    /// A task pushed on one thread reaches an inbox that waits on another.
    #[test]
    fn a_task_pushed_on_one_thread_reaches_another() {
        loom::model(|| {
            let (queue, inbox) = pair();
            let shard = thread::spawn(move || {
                block_on(std::future::poll_fn(|cx| inbox.poll(cx)))
            });
            queue.push(7);
            assert_eq!(shard.join().unwrap(), vec![7]);
        });
    }

    /// A task pushed as the inbox drops on another thread is taken or dropped, never
    /// kept.
    #[test]
    fn a_task_pushed_as_the_inbox_drops_is_dropped() {
        loom::model(|| {
            let (queue, inbox) = pair();
            let task = std::sync::Arc::new(());
            let pushed = std::sync::Arc::clone(&task);
            let shard = thread::spawn(move || drop(inbox));
            queue.push(pushed);
            shard.join().unwrap();
            assert_eq!(std::sync::Arc::strong_count(&task), 1);
        });
    }
}
