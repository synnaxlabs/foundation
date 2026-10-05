//! The state of one run, shared by the scheduler and the drivers.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Wake, Waker};
use std::time::Instant;

use env::rng::Rng;
use env::shards::Main;
use env::tasks::Task;
use env::threads::{Body, Error};
use types::time::{Monotonic, Span, Stamp};

use crate::node;

pub(crate) type Shared = Arc<Mutex<State>>;

/// Locks the state. No code that can panic, and no code from outside the crate, runs
/// while it holds the lock, so the lock is never poisoned and no driver call blocks
/// on it.
pub(crate) fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared
        .lock()
        .expect("invariant: nothing panics while it holds the sim state")
}

pub(crate) struct State {
    /// True time: zero when the run starts.
    now: Monotonic,
    /// The end of true time: the last instant at which every node's clocks can be
    /// read. It never falls below `now`.
    last: Monotonic,
    nodes: Vec<Node>,
    threads: BTreeMap<u64, Thread>,
    /// The thread of each live task.
    tasks: BTreeMap<u64, u64>,
    ready: BTreeSet<u64>,
    /// Wakers by true deadline, then timer key.
    timers: BTreeMap<(Monotonic, u64), Waker>,
    /// The first task of each thread that has not run yet.
    starts: BTreeMap<u64, Start>,
    /// The thread whose task the scheduler polls now: set by `pick`, cleared by
    /// `release`.
    current: Option<u64>,
    /// The `Instant` at `Monotonic(0)` on every node.
    epoch: Instant,
    next: u64,
}

struct Node {
    /// True time when the node was added.
    added: Monotonic,
    monotonic: Monotonic,
    wall: Stamp,
    cores: NonZeroUsize,
    entropy: Rng,
}

impl Node {
    /// The true time at which the first of the node's clocks reaches its end.
    fn last(&self) -> Monotonic {
        let monotonic = u64::MAX - self.monotonic.0;
        let wall = u64::try_from(i128::from(i64::MAX) - i128::from(self.wall.nanos()))
            .expect("invariant: i64::MAX minus an i64 fits u64");
        Monotonic(self.added.0.saturating_add(monotonic.min(wall)))
    }
}

struct Thread {
    name: String,
    node: usize,
    main: u64,
    outcome: Option<Result<(), Error>>,
}

/// The next step of a run.
pub(crate) enum Next {
    /// Poll a ready task.
    Poll,
    /// Move true time to the first timer.
    Fire(Monotonic),
}

/// When a timer fires.
pub(crate) enum Due {
    /// Its deadline has passed.
    Passed,
    /// At this true time.
    At(Monotonic),
    /// Never: its deadline is past the end of true time.
    Never,
}

pub(crate) enum Start {
    Shard(Main),
    Body(Body),
}

impl State {
    pub(crate) fn new(epoch: Instant) -> Self {
        Self {
            now: Monotonic::default(),
            last: Monotonic(u64::MAX),
            nodes: Vec::new(),
            threads: BTreeMap::new(),
            tasks: BTreeMap::new(),
            ready: BTreeSet::new(),
            timers: BTreeMap::new(),
            starts: BTreeMap::new(),
            current: None,
            epoch,
            next: 0,
        }
    }

    /// A new key for a thread, a task, or a timer.
    pub(crate) fn key(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Adds a node whose clocks read the values in `config` now, and returns its
    /// index.
    pub(crate) fn add(&mut self, config: node::Config, entropy: Rng) -> usize {
        let node = Node {
            added: self.now,
            monotonic: config.monotonic,
            wall: config.wall,
            cores: config.cores,
            entropy,
        };
        self.last = self.last.min(node.last());
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// True time `span` from now, or `None` past the end of true time.
    pub(crate) fn after(&self, span: Span) -> Option<Monotonic> {
        self.now.checked_add(span).filter(|&end| end <= self.last)
    }

    /// True time since `node` was added.
    fn since(&self, node: usize) -> u64 {
        self.now.0 - self.nodes[node].added.0
    }

    pub(crate) fn monotonic(&self, node: usize) -> Monotonic {
        Monotonic(self.nodes[node].monotonic.0 + self.since(node))
    }

    pub(crate) fn wall(&self, node: usize) -> Stamp {
        let nanos =
            i128::from(self.nodes[node].wall.nanos()) + i128::from(self.since(node));
        let nanos = i64::try_from(nanos)
            .expect("invariant: true time ends before a wall clock");
        Stamp::from_nanos(nanos)
    }

    pub(crate) fn cores(&self, node: usize) -> NonZeroUsize {
        self.nodes[node].cores
    }

    pub(crate) fn fill(&mut self, node: usize, bytes: &mut [u8]) {
        self.nodes[node].entropy.fill(bytes);
    }

    /// When a timer of `node` with `deadline` fires.
    pub(crate) fn due(&self, node: usize, deadline: Monotonic) -> Due {
        let now = self.monotonic(node);
        if deadline <= now {
            return Due::Passed;
        }
        match self.now.0.checked_add(deadline.0 - now.0) {
            Some(at) if at <= self.last.0 => Due::At(Monotonic(at)),
            _ => Due::Never,
        }
    }

    /// Adds timer `key`, which wakes `waker` at true time `at`.
    pub(crate) fn arm(&mut self, at: Monotonic, key: u64, waker: Waker) {
        self.timers.insert((at, key), waker);
    }

    /// Removes timer `key` at true time `at`, and returns its waker for the caller to
    /// drop after it releases the lock.
    pub(crate) fn disarm(&mut self, at: Monotonic, key: u64) -> Option<Waker> {
        self.timers.remove(&(at, key))
    }

    /// The node of the thread that the scheduler polls now, if any.
    pub(crate) fn current(&self) -> Option<usize> {
        self.current.map(|thread| self.threads[&thread].node)
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

    pub(crate) fn name(&self, thread: u64) -> String {
        self.threads[&thread].name.clone()
    }

    /// How `thread` ended, or `None` while it runs.
    pub(crate) fn outcome(&self, thread: u64) -> Option<Result<(), Error>> {
        self.threads[&thread].outcome.clone()
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

    /// Ends the poll that `pick` began.
    pub(crate) fn release(&mut self) {
        self.current = None;
    }

    /// Removes `task` of `thread`, which completed; it may have woken itself as it
    /// completed. The first task of a thread ends the thread. Returns the tasks whose
    /// futures the caller drops.
    pub(crate) fn finish(&mut self, task: u64, thread: u64) -> Vec<u64> {
        if task == self.threads[&thread].main {
            return self.end(thread, Ok(()));
        }
        self.tasks.remove(&task);
        self.ready.remove(&task);
        vec![task]
    }

    /// Ends `thread` with `outcome` and returns the keys of its tasks, whose futures
    /// the caller drops.
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
        let mut wakers = Vec::new();
        while let Some(timer) = self.timers.first_entry() {
            if timer.key().0 > due {
                break;
            }
            wakers.push(timer.remove());
        }
        wakers
    }

    /// Removes the starts of the threads that have not run.
    pub(crate) fn unstarted(&mut self) -> BTreeMap<u64, Start> {
        mem::take(&mut self.starts)
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
