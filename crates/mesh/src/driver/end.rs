//! Spawns the tasks of a mesh, and tells when each has ended.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use env::tasks::Tasks;

/// Spawns each task of one mesh, and counts it until it ends.
#[derive(Clone)]
pub(super) struct Spawner {
    tasks: Tasks,
    count: Rc<Count>,
}

impl Spawner {
    pub(super) fn new(tasks: Tasks) -> Self {
        Self {
            tasks,
            count: Rc::default(),
        }
    }

    pub(super) fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        let live = Live::new(&self.count);
        self.tasks.spawn(async move {
            task.await;
            drop(live);
        });
    }

    pub(super) fn ended(&self) -> Ended {
        let slot = self.count.slots.get();
        self.count.slots.set(slot.wrapping_add(1));
        Ended {
            count: Rc::clone(&self.count),
            slot,
        }
    }
}

// The tasks of one mesh that have not ended.
#[derive(Default)]
struct Count {
    tasks: Cell<usize>,
    // The waker of each `Ended` that waits, by its slot.
    waiting: RefCell<BTreeMap<u64, Waker>>,
    // The slot of the next `Ended`.
    slots: Cell<u64>,
}

// Held by one task until it ends.
struct Live(Rc<Count>);

impl Live {
    fn new(count: &Rc<Count>) -> Self {
        count.tasks.set(count.tasks.get().strict_add(1));
        Self(Rc::clone(count))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let tasks = self.0.tasks.get().strict_sub(1);
        self.0.tasks.set(tasks);
        if tasks == 0 {
            self.0.waiting.take().into_values().for_each(Waker::wake);
        }
    }
}

/// Resolves once each task of a [`Mesh`](super::Mesh) has ended.
/// [`Mesh::ended`](super::Mesh::ended) gives it.
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
        if self.count.tasks.get() == 0 {
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        self.count.waiting.borrow_mut().insert(self.slot, waker);
        Poll::Pending
    }
}

impl Drop for Ended {
    fn drop(&mut self) {
        self.count.waiting.borrow_mut().remove(&self.slot);
    }
}
