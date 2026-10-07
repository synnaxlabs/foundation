//! The cost of one poll of a future through a `Scope`, as `node` polls each stream
//! future. Run with `cargo bench -p node --bench scope`.
//!
//! Each line polls a future that counts its polls and stays pending:
//!
//! - `bare`: the control, a direct poll of the boxed future.
//! - `same_waker`: a poll through the scope with the same waker each time, as Tokio and
//!   `sim` give.
//! - `new_waker`: a poll through the scope with one of two wakers in turn, so the
//!   scope clones the waker at each poll.
//!
//! `spawn` times the spawn and completion of a future that is ready at its first poll.
//!
//! Judge the scope by a line's time less `bare`'s, from one run of one binary.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::{Cell, RefCell};
use std::future::{poll_fn, ready};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use divan::Bencher;
use env::tasks::Task;
use node::bench::Scope;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const POLLS: u64 = 1_000_000;

fn main() {
    check();
    divan::main();
}

/// Keeps each task for the bench to poll.
#[derive(Clone, Default)]
struct Queued(Rc<RefCell<Vec<Task>>>);

impl env::tasks::Driver for Queued {
    fn spawn(&self, task: Task) {
        self.0.borrow_mut().push(task);
    }
}

impl Queued {
    /// The one task spawned since the last call.
    fn take(&self) -> Task {
        let mut tasks = self.0.borrow_mut();
        assert_eq!(tasks.len(), 1, "one task per spawn");
        tasks.pop().expect("one task")
    }
}

/// Whether its waker has woken. Each is a distinct waker, unlike `Waker::noop`.
#[derive(Default)]
struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A future that counts its polls in `polls` and never completes.
fn counted(polls: &Rc<Cell<u64>>) -> Task {
    let polls = Rc::clone(polls);
    Box::pin(poll_fn(move |_| {
        polls.set(polls.get() + 1);
        Poll::<()>::Pending
    }))
}

/// A scope that runs a counted future, and the task that polls it through the scope.
fn scoped(polls: &Rc<Cell<u64>>) -> (Scope, Task) {
    let queued = Queued::default();
    let mut scope = Scope::new(env::tasks::Tasks::new(queued.clone()));
    scope.spawn(counted(polls));
    (scope, queued.take())
}

/// Polls `task` `count` times, with `wakers` in turn, and asserts it stays pending.
fn poll(task: &mut Pin<Box<dyn Future<Output = ()>>>, wakers: &[Waker], count: u64) {
    for (_, waker) in (0..count).zip(wakers.iter().cycle()) {
        let mut cx = Context::from_waker(waker);
        let polled = divan::black_box(task.as_mut().poll(&mut cx));
        assert!(polled.is_pending(), "the counted future never completes");
    }
}

fn wakers(count: usize) -> Vec<Waker> {
    (0..count)
        .map(|_| Waker::from(Arc::new(Woken::default())))
        .collect()
}

/// Each poll through the scope polls its future once, and allocates nothing with the
/// same waker. A drop of the scope wakes the last waker and ends the task.
fn check() {
    let polls = Rc::new(Cell::new(0));
    let (scope, mut task) = scoped(&polls);
    assert_eq!(format!("{scope:?}"), "Scope", "the scope's debug form");
    let same = wakers(1);
    poll(&mut task, &same, 1);
    let ((), allocations) = ALLOCATOR.count(|| poll(&mut task, &same, 1000));
    assert_eq!(
        (polls.get(), allocations),
        (1001, 0),
        "polls and allocations"
    );
    let woken = Arc::new(Woken::default());
    let last = Waker::from(Arc::clone(&woken));
    poll(&mut task, &[wakers(1).remove(0), last], 1000);
    assert_eq!(
        polls.get(),
        2001,
        "one poll of the future per poll of the task"
    );
    drop(scope);
    assert!(
        woken.0.load(Ordering::Relaxed),
        "the drop wakes the last waker"
    );
    let mut cx = Context::from_waker(Waker::noop());
    let polled = task.as_mut().poll(&mut cx);
    assert!(polled.is_ready(), "the task ends once the scope drops");
    assert_eq!(polls.get(), 2001, "the future is not polled after the drop");
}

#[divan::bench(sample_count = 20)]
fn bare(bencher: Bencher<'_, '_>) {
    let polls = Rc::new(Cell::new(0));
    let mut task = counted(&polls);
    let wakers = wakers(1);
    bencher
        .counter(divan::counter::ItemsCount::new(POLLS))
        .bench_local(|| poll(&mut task, &wakers, POLLS));
}

#[divan::bench(sample_count = 20)]
fn same_waker(bencher: Bencher<'_, '_>) {
    let polls = Rc::new(Cell::new(0));
    let (_scope, mut task) = scoped(&polls);
    let wakers = wakers(1);
    bencher
        .counter(divan::counter::ItemsCount::new(POLLS))
        .bench_local(|| poll(&mut task, &wakers, POLLS));
}

#[divan::bench(sample_count = 20)]
fn new_waker(bencher: Bencher<'_, '_>) {
    let polls = Rc::new(Cell::new(0));
    let (_scope, mut task) = scoped(&polls);
    let wakers = wakers(2);
    bencher
        .counter(divan::counter::ItemsCount::new(POLLS))
        .bench_local(|| poll(&mut task, &wakers, POLLS));
}

#[divan::bench(sample_count = 20)]
fn spawn(bencher: Bencher<'_, '_>) {
    let queued = Queued::default();
    let mut scope = Scope::new(env::tasks::Tasks::new(queued.clone()));
    let waker = Waker::noop();
    bencher.bench_local(|| {
        scope.spawn(Box::pin(ready(())));
        let mut task = queued.take();
        let mut cx = Context::from_waker(waker);
        let polled = task.as_mut().poll(&mut cx);
        assert!(
            polled.is_ready(),
            "the ready future completes at its first poll"
        );
    });
}
