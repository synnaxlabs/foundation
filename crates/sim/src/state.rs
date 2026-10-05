//! The state of one run, shared by the scheduler and the drivers.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Wake, Waker};
use std::thread::ThreadId;
use std::time::Instant;

use env::rng::Rng;
use env::shards::Main;
use env::tasks::Task;
use env::threads::{Body, Error};
use types::time::{Monotonic, Span, Stamp};

pub(crate) type Shared = Arc<Mutex<State>>;

/// Locks the state. Nothing runs code from outside the crate while it holds the lock,
/// so the lock is never poisoned, and no driver call blocks on it.
pub(crate) fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared
        .lock()
        .expect("invariant: nothing panics while it holds the sim state")
}

pub(crate) struct State {
    /// True time: zero when the run starts.
    pub(crate) now: Monotonic,
    pub(crate) nodes: Vec<Node>,
    pub(crate) threads: BTreeMap<u64, Thread>,
    /// The thread of each live task.
    pub(crate) tasks: BTreeMap<u64, u64>,
    pub(crate) ready: BTreeSet<u64>,
    /// Wakers by true deadline, then timer key.
    pub(crate) timers: BTreeMap<(Monotonic, u64), Waker>,
    /// The first task of each thread that has not run yet.
    pub(crate) starts: BTreeMap<u64, Start>,
    /// The thread whose task the scheduler polls now.
    pub(crate) current: Option<u64>,
    /// The OS thread that runs the scheduler.
    pub(crate) runner: ThreadId,
    /// The `Instant` at `Monotonic(0)` on every node.
    pub(crate) epoch: Instant,
    next: u64,
}

pub(crate) struct Node {
    pub(crate) monotonic: Monotonic,
    pub(crate) wall: Stamp,
    pub(crate) cores: NonZeroUsize,
    pub(crate) entropy: Rng,
}

pub(crate) struct Thread {
    pub(crate) name: String,
    pub(crate) node: usize,
    pub(crate) main: u64,
    pub(crate) outcome: Option<Result<(), Error>>,
}

/// The next step of a run.
pub(crate) enum Next {
    /// Poll a ready task.
    Poll,
    /// Move true time to the first timer.
    Fire(Monotonic),
}

pub(crate) enum Start {
    Shard(Main),
    Body(Body),
}

impl State {
    pub(crate) fn new(runner: ThreadId, epoch: Instant) -> Self {
        Self {
            now: Monotonic::default(),
            nodes: Vec::new(),
            threads: BTreeMap::new(),
            tasks: BTreeMap::new(),
            ready: BTreeSet::new(),
            timers: BTreeMap::new(),
            starts: BTreeMap::new(),
            current: None,
            runner,
            epoch,
            next: 0,
        }
    }

    /// A new key for a thread, a task, or a timer.
    pub(crate) fn key(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// True time since the run started.
    pub(crate) fn elapsed(&self) -> Span {
        self.now - Monotonic::default()
    }

    pub(crate) fn monotonic(&self, node: usize) -> Monotonic {
        self.nodes[node].monotonic + self.elapsed()
    }

    /// Adds a thread whose first task is ready, and returns the thread's key.
    pub(crate) fn start(&mut self, node: usize, name: String, start: Start) -> u64 {
        let thread = self.key();
        let main = self.key();
        let outcome = None;
        self.threads.insert(
            thread,
            Thread {
                name,
                node,
                main,
                outcome,
            },
        );
        self.tasks.insert(main, thread);
        self.ready.insert(main);
        self.starts.insert(main, start);
        thread
    }

    /// Adds a ready task to `thread` and returns its key, or `None` when the thread
    /// has ended.
    pub(crate) fn spawn(&mut self, thread: u64) -> Option<u64> {
        self.threads[&thread].outcome.is_none().then(|| {
            let task = self.key();
            self.tasks.insert(task, thread);
            self.ready.insert(task);
            task
        })
    }

    /// The next step of a run that stops at true time `end`, or `None` when it stops
    /// there or when no task is ready and no timer waits. A run that stops at `end`
    /// is at `end`.
    pub(crate) fn next(&mut self, end: Option<Monotonic>) -> Option<Next> {
        if !self.ready.is_empty() {
            return Some(Next::Poll);
        }
        let due = self.timers.first_key_value().map(|(&(due, _), _)| due);
        match (due, end) {
            (Some(due), Some(end)) if due > end => {
                self.now = end;
                None
            }
            (Some(due), _) => Some(Next::Fire(due)),
            (None, Some(end)) => {
                self.now = end;
                None
            }
            (None, None) => None,
        }
    }

    /// Takes a ready task at random and makes its thread the current one. Returns
    /// the task, its thread, and the thread's start when the task is its first.
    ///
    /// # Panics
    ///
    /// When no task is ready.
    pub(crate) fn pick(&mut self, rng: &mut Rng) -> (u64, u64, Option<Start>) {
        let ready = u64::try_from(self.ready.len()).expect("invariant: usize fits u64");
        let nth = usize::try_from(rng.below(ready))
            .expect("invariant: a value below a usize fits usize");
        let task = *(self.ready.iter().nth(nth)).expect("invariant: nth is below len");
        self.ready.remove(&task);
        let thread = self.tasks[&task];
        self.current = Some(thread);
        (task, thread, self.starts.remove(&task))
    }

    /// Removes a task that completed. It may have woken itself as it completed.
    pub(crate) fn finish(&mut self, task: u64) {
        self.tasks.remove(&task);
        self.ready.remove(&task);
    }

    /// Ends `thread` and returns the keys of its tasks, whose futures the caller
    /// drops.
    pub(crate) fn end(&mut self, thread: u64, outcome: Result<(), Error>) -> Vec<u64> {
        self.threads
            .get_mut(&thread)
            .expect("invariant: an ending thread exists")
            .outcome = Some(outcome);
        let tasks: Vec<u64> = (self.tasks.iter())
            .filter_map(|(&task, &owner)| (owner == thread).then_some(task))
            .collect();
        for task in &tasks {
            self.tasks.remove(task);
            self.ready.remove(task);
        }
        tasks
    }

    /// Moves true time to `due` and returns the wakers of the timers due by then.
    pub(crate) fn advance(&mut self, due: Monotonic) -> Vec<Waker> {
        self.now = due;
        let later = self.timers.split_off(&(due + Span::NANOSECOND, 0));
        mem::replace(&mut self.timers, later)
            .into_values()
            .collect()
    }

    /// The names of the threads that have not ended, in start order.
    pub(crate) fn live(&self) -> Vec<String> {
        (self.threads.values())
            .filter(|thread| thread.outcome.is_none())
            .map(|thread| thread.name.clone())
            .collect()
    }
}

/// Wakes one task: it puts the task in the ready set while the task lives.
struct TaskWaker {
    shared: Weak<Mutex<State>>,
    task: u64,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(shared) = self.shared.upgrade() {
            let mut state = lock(&shared);
            if state.tasks.contains_key(&self.task) {
                state.ready.insert(self.task);
            }
        }
    }
}

/// The futures of the live tasks. They are not `Send`, so they stay with the
/// scheduler on its thread, out of [`State`].
#[derive(Default)]
pub(crate) struct Futures(BTreeMap<u64, Entry>);

struct Entry {
    future: Option<Task>,
    waker: Waker,
}

impl Futures {
    /// Adds the future of `task`, with a waker that makes it ready.
    pub(crate) fn insert(&mut self, shared: &Shared, task: u64, future: Task) {
        let waker = Waker::from(Arc::new(TaskWaker {
            shared: Arc::downgrade(shared),
            task,
        }));
        let future = Some(future);
        self.0.insert(task, Entry { future, waker });
    }

    /// Takes the future of `task` out to poll it, with its waker.
    pub(crate) fn take(&mut self, task: u64) -> (Task, Waker) {
        let entry = self
            .0
            .get_mut(&task)
            .expect("invariant: a live task has an entry");
        let future = (entry.future.take()).expect("invariant: one poll at a time");
        (future, entry.waker.clone())
    }

    /// Puts back the future of `task` after a poll that returned `Pending`.
    pub(crate) fn put(&mut self, task: u64, future: Task) {
        let entry = self
            .0
            .get_mut(&task)
            .expect("invariant: a live task has an entry");
        entry.future = Some(future);
    }

    /// Removes the entries of `tasks` and returns their futures, which the caller
    /// drops after it releases the borrow.
    pub(crate) fn remove(&mut self, tasks: &[u64]) -> Vec<Task> {
        (tasks.iter())
            .filter_map(|task| self.0.remove(task)?.future)
            .collect()
    }

    /// Removes every entry and returns the futures.
    pub(crate) fn clear(&mut self) -> Vec<Task> {
        mem::take(&mut self.0)
            .into_values()
            .filter_map(|entry| entry.future)
            .collect()
    }
}
