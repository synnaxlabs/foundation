//! A turn that one holder has at a time.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// One holder at a time, given in the order that holders ask for it.
#[derive(Debug, Default)]
pub(super) struct Turn {
    /// Stays true while waiters remain, as `give` hands the turn on directly.
    taken: Cell<bool>,
    waiters: RefCell<VecDeque<Rc<Waiter>>>,
}

#[derive(Debug, Default)]
struct Waiter {
    given: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

/// The turn, which a drop gives to the next holder.
#[derive(Debug)]
pub(super) struct Taken(Rc<Turn>);

impl Turn {
    /// Waits for the turn and takes it.
    pub(super) async fn take(self: &Rc<Self>) -> Taken {
        if self.taken.replace(true) {
            let waiter = Rc::new(Waiter::default());
            self.waiters.borrow_mut().push_back(Rc::clone(&waiter));
            Wait {
                turn: self,
                waiter,
                done: false,
            }
            .await;
        }
        Taken(Rc::clone(self))
    }

    fn give(&self) {
        let next = self.waiters.borrow_mut().pop_front();
        let Some(waiter) = next else {
            self.taken.set(false);
            return;
        };
        waiter.given.set(true);
        if let Some(waker) = waiter.waker.take() {
            waker.wake();
        }
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        self.0.give();
    }
}

/// A place in the line for the turn. A drop leaves the line, or hands on a turn
/// given to it.
struct Wait<'a> {
    turn: &'a Turn,
    waiter: Rc<Waiter>,
    done: bool,
}

impl Future for Wait<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.waiter.given.get() {
            self.done = true;
            return Poll::Ready(());
        }
        *self.waiter.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if self.waiter.given.get() {
            self.turn.give();
        } else {
            let waiter = &self.waiter;
            self.turn
                .waiters
                .borrow_mut()
                .retain(|other| !Rc::ptr_eq(other, waiter));
        }
    }
}
