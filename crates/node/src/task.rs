//! Tasks given from any thread that shard 0 calls with its hub, in the order given.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::mem;
use std::pin::{Pin, pin};
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
    fn poll(&self, cx: &mut Context<'_>) -> Poll<Vec<T>> {
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

impl Inbox<Task> {
    /// Calls each task given with a clone of `hub`, in the order given, and runs its
    /// future on `tasks`, until `stop` completes. Then drops each future, `hub`, and
    /// the inbox. A future that completes drops at once.
    pub(crate) async fn serve(
        self,
        hub: Hub,
        tasks: env::tasks::Tasks,
        stop: impl Future<Output = ()>,
    ) {
        let running: Rc<RefCell<hash::Map<u64, Slot>>> = Rc::default();
        let mut next = 0;
        let mut stop = pin!(stop);
        poll_fn(|cx| {
            if stop.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            while let Poll::Ready(given) = self.poll(cx) {
                for task in given {
                    // A task body can stop the node.
                    if stop.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(());
                    }
                    let slot = Rc::new(RefCell::new(task(hub.clone())));
                    let run = Spawned {
                        slot: Rc::downgrade(&slot),
                        running: Rc::downgrade(&running),
                        number: next,
                    };
                    running.borrow_mut().insert(next, slot);
                    next += 1;
                    tasks.spawn(poll_fn(move |cx| run.poll(cx)));
                }
            }
            Poll::Pending
        })
        .await;
        drop(running);
    }
}

/// The future of one running task. Only the map of running futures holds it, so a
/// stop drops it.
type Slot = Rc<RefCell<Boxed>>;

/// What the executor holds of one running task.
struct Spawned {
    slot: Weak<RefCell<Boxed>>,
    running: Weak<RefCell<hash::Map<u64, Slot>>>,
    /// The task's key in `running`.
    number: u64,
}

impl Spawned {
    /// Polls the task, then drops it from `running` once it completes. Ends once the
    /// task has dropped.
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

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::sync::Arc;

    use super::pair;

    #[test]
    fn a_task_pushed_after_the_inbox_drops_is_dropped() {
        let (queue, inbox) = pair();
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
