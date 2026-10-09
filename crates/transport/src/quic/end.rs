//! Tells when the task of a carrier has dropped, and its socket with it.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// What the task of a carrier shares with each [`Ended`] of it.
#[derive(Default)]
pub(crate) struct End {
    ended: Cell<bool>,
    /// The waker of each [`Ended`] that waits, by its slot.
    waiting: RefCell<BTreeMap<u64, Waker>>,
    /// The slot of the next [`Ended`].
    slots: Cell<u64>,
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
        self.0.waiting.take().into_values().for_each(Waker::wake);
    }
}

/// Resolves once a [`Transport`](crate::Transport) has freed its
/// [`port::Part`](crate::port::Part). [`Transport::ended`](crate::Transport::ended)
/// gives it.
pub struct Ended {
    end: Rc<End>,
    slot: u64,
}

impl Ended {
    pub(crate) fn new(end: &Rc<End>) -> Self {
        let slot = end.slots.get();
        end.slots.set(slot.strict_add(1));
        Self {
            end: Rc::clone(end),
            slot,
        }
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
        if self.end.ended.get() {
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        self.end.waiting.borrow_mut().insert(self.slot, waker);
        Poll::Pending
    }
}

impl Drop for Ended {
    fn drop(&mut self) {
        self.end.waiting.borrow_mut().remove(&self.slot);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use super::*;

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn the_drop_of_the_live_wakes_only_each_ended_that_still_waits() {
        let end = Rc::new(End::default());
        let live = Live::new(&end);
        let kept = Arc::new(Count(AtomicUsize::new(0)));
        let dropped = Arc::new(Count(AtomicUsize::new(0)));
        let mut ended = Ended::new(&end);
        let mut gone = Ended::new(&end);
        for (ended, count) in [(&mut ended, &kept), (&mut gone, &dropped)] {
            let waker = Waker::from(Arc::clone(count));
            let poll = Pin::new(ended).poll(&mut Context::from_waker(&waker));
            assert_eq!(poll, Poll::Pending);
        }
        drop(gone);
        drop(live);
        assert_eq!(kept.0.load(Ordering::Relaxed), 1);
        assert_eq!(dropped.0.load(Ordering::Relaxed), 0);
        let waker = Waker::noop();
        let poll = Pin::new(&mut ended).poll(&mut Context::from_waker(waker));
        assert_eq!(poll, Poll::Ready(()));
    }
}
