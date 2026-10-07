//! The commit task: it wakes complete readers after each group commit that holds
//! frames for them.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::pin::Pin;
use std::rc::Weak;
use std::task::{Poll, Waker};

/// The commit task's part of the state: whether a commit is due, the commit it waits
/// for, and its waker.
#[derive(Debug, Default)]
pub(crate) struct Signal {
    /// Whether a home call may have appended since the task last waited for a commit.
    pub(crate) due: bool,
    /// The commit that the task waits for, which wakes it when it resolves. It holds
    /// the ring open, so it drops with the state.
    pending: Option<home::Commit>,
    /// The task's waker, kept while it sleeps and while it waits for a commit.
    pub(crate) task: Option<Waker>,
}

impl Signal {
    /// Notes a home call that may have appended to the buffer: a write, also a failed
    /// one, and the handoff of a writer open or close. Wakes the task only when it
    /// sleeps, once.
    pub(crate) fn appended(&mut self) {
        if !mem::replace(&mut self.due, true)
            && self.pending.is_none()
            && let Some(task) = &self.task
        {
            task.wake_by_ref();
        }
    }
}

/// Waits for each commit that holds frames for complete readers, then wakes them.
/// After a failed commit, it keeps the error, wakes every reader, and ends. It holds
/// `state` only during a poll, and its waker stays in the state in each phase, so the
/// first poll after the state drops ends it.
pub(crate) async fn run(state: Weak<RefCell<super::State>>) {
    poll_fn(|cx| {
        loop {
            let Some(state) = state.upgrade() else {
                return Poll::Ready(());
            };
            let mut state = state.borrow_mut();
            if !state
                .commit
                .task
                .as_ref()
                .is_some_and(|task| task.will_wake(cx.waker()))
            {
                state.commit.task = Some(cx.waker().clone());
            }
            if let Some(pending) = state.commit.pending.as_mut() {
                let Poll::Ready(committed) = Pin::new(pending).poll(cx) else {
                    return Poll::Pending;
                };
                state.commit.pending = None;
                match committed {
                    Ok(()) => state.wake(),
                    Err(error) => {
                        state.fail(error);
                        return Poll::Ready(());
                    }
                }
            }
            // A commit future resolves at once when nothing waits, so the task sleeps
            // until a session gives it an append to wait for.
            if !mem::take(&mut state.commit.due) {
                return Poll::Pending;
            }
            state.commit.pending = Some(state.home.committed());
        }
    })
    .await;
}

impl Drop for Signal {
    /// Wakes the task, which then finds the state gone and ends.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }
}
