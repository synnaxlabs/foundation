//! A one-time signal that every shard of a node waits on.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// Set once, from any thread. Each shard touches it once to wait and once to end,
/// never on a frame's path, so a mutex is fine.
#[derive(Debug, Default)]
pub(crate) struct Stop(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    set: bool,
    wakers: Vec<Waker>,
}

impl Stop {
    /// Sets the signal and wakes every guard. Later calls do nothing.
    pub(crate) fn set(&self) {
        let wakers = {
            let mut state = self.lock();
            state.set = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// A shard's hold on the signal: it completes once the signal is set, and it sets
    /// the signal when dropped, so any shard that ends stops the node.
    pub(crate) fn guard(self: &Arc<Self>) -> Guard {
        Guard {
            stop: Arc::clone(self),
            slot: None,
        }
    }

    /// Nothing panics while it holds the lock. `into_inner` keeps `Guard::drop` from
    /// panicking all the same.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The future of [`Stop::guard`]. It keeps one waker slot, so a repeated poll does not
/// grow the list.
#[derive(Debug)]
pub(crate) struct Guard {
    stop: Arc<Stop>,
    slot: Option<usize>,
}

impl Future for Guard {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.stop.lock();
        if state.set {
            return Poll::Ready(());
        }
        if let Some(slot) = self.slot {
            state.wakers[slot].clone_from(cx.waker());
        } else {
            state.wakers.push(cx.waker().clone());
            let slot = state.wakers.len() - 1;
            drop(state);
            self.slot = Some(slot);
        }
        Poll::Pending
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.set();
    }
}
