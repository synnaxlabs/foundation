//! The commit task: it wakes complete readers after each group commit that holds
//! frames for them.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::rc::Weak;
use std::task::{Poll, Waker};

/// How the sessions tell the commit task that a commit is due.
#[derive(Debug, Default)]
pub(crate) struct Signal {
    /// Whether a home call may have appended since the task last waited for a commit.
    pub(crate) due: bool,
    /// The task's waker while it sleeps.
    pub(crate) task: Option<Waker>,
}

impl Signal {
    /// Notes a home call that may have appended to the buffer: a write, also a failed
    /// one, and the handoff of a writer open or close. Wakes the task once per commit
    /// at most.
    pub(crate) fn appended(&mut self) {
        self.due = true;
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }
}

/// Waits for each commit that holds frames for complete readers, then wakes them.
/// After a failed commit, it keeps the error, wakes every reader, and ends. It holds
/// `state` only during a poll, so it ends once the hub and each session drop.
pub(crate) async fn run(state: Weak<RefCell<super::State>>) {
    loop {
        // A commit future resolves at once when nothing waits, so the task sleeps
        // until a session gives it an append to wait for.
        let commit = poll_fn(|cx| {
            let Some(state) = state.upgrade() else {
                return Poll::Ready(None);
            };
            let mut state = state.borrow_mut();
            if mem::take(&mut state.commit.due) {
                return Poll::Ready(Some(state.home.committed()));
            }
            state.commit.task = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        let Some(commit) = commit else { return };
        let committed = commit.await;
        let Some(state) = state.upgrade() else { return };
        let mut state = state.borrow_mut();
        match committed {
            Ok(()) => state.wake(),
            Err(error) => return state.fail(error),
        }
    }
}
