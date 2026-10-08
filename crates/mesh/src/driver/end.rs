//! Tells when each task of a group has ended.

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// The tasks of one group that have not ended. Each holds a [`Live`].
#[derive(Debug, Default)]
pub(super) struct Running {
    count: Cell<usize>,
    waiting: RefCell<Vec<Waker>>,
}

/// Held by one task of a group until it ends. A clone counts as one more task.
#[derive(Debug)]
pub(super) struct Live(Rc<Running>);

impl Live {
    pub(super) fn new(running: &Rc<Running>) -> Self {
        running.count.set(running.count.get().strict_add(1));
        Self(Rc::clone(running))
    }
}

impl Clone for Live {
    fn clone(&self) -> Self {
        Self::new(&self.0)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let count = self.0.count.get().strict_sub(1);
        self.0.count.set(count);
        if count == 0 {
            self.0.waiting.take().into_iter().for_each(Waker::wake);
        }
    }
}

/// Resolves once each task of a [`Mesh`](super::Mesh) has ended.
/// [`Mesh::ended`](super::Mesh::ended) gives it.
#[derive(Debug)]
pub struct Ended(pub(super) Rc<Running>);

impl Future for Ended {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.count.get() == 0 {
            return Poll::Ready(());
        }
        let mut waiting = self.0.waiting.borrow_mut();
        waiting.retain(|waker| !waker.will_wake(cx.waker()));
        waiting.push(cx.waker().clone());
        Poll::Pending
    }
}
