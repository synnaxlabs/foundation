//! Tells when the task of a carrier has dropped, and its socket with it.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::wake::register;

/// What the task of a carrier shares with each [`Ended`] of it.
#[derive(Default)]
pub(crate) struct End {
    ended: Cell<bool>,
    /// The wakers of the [`Ended`] futures that wait.
    waiting: RefCell<Vec<Waker>>,
}

/// Held by the task of a carrier. Its drop resolves each [`Ended`] of its [`End`].
pub(crate) struct Live(Rc<End>);

impl Live {
    pub(crate) fn new(end: &Rc<End>) -> Self {
        Self(Rc::clone(end))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.ended.set(true);
        self.0.waiting.take().into_iter().for_each(Waker::wake);
    }
}

/// Resolves once a [`Transport`](crate::Transport) has freed its
/// [`port::Part`](crate::port::Part). [`Transport::ended`](crate::Transport::ended)
/// gives it.
pub struct Ended(Rc<End>);

impl Ended {
    pub(crate) fn new(end: &Rc<End>) -> Self {
        Self(Rc::clone(end))
    }
}

impl fmt::Debug for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ended").finish_non_exhaustive()
    }
}

impl Future for Ended {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.ended.get() {
            return Poll::Ready(());
        }
        register(&mut self.0.waiting.borrow_mut(), cx.waker());
        Poll::Pending
    }
}
