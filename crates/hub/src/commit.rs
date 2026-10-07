//! The commit task: it wakes complete readers after each group commit that holds
//! frames for them.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::rc::Rc;
use std::task::{Poll, Waker};

/// What the commit task and the sessions share.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// Whether a write took a frame since the task last waited for a commit.
    pub(crate) waiting: bool,
    /// The task's waker while it sleeps.
    pub(crate) task: Option<Waker>,
}

impl State {
    /// Notes a write that the home took. Wakes the task once per commit at most.
    pub(crate) fn written(&mut self) {
        self.waiting = true;
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }
}

/// Waits for each commit that holds frames for complete readers, then wakes them.
/// After a failed commit, it keeps the error, wakes every reader, and ends.
pub(crate) async fn run(state: Rc<RefCell<super::State>>) {
    loop {
        // A commit future resolves at once when no frame waits, so the task sleeps
        // until a write gives it one to wait for.
        let commit = poll_fn(|cx| {
            let mut state = state.borrow_mut();
            if mem::take(&mut state.commit.waiting) {
                return Poll::Ready(state.home.committed());
            }
            state.commit.task = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        let committed = commit.await;
        let mut state = state.borrow_mut();
        match committed {
            Ok(()) => state.wake(),
            Err(error) => return state.fail(error),
        }
    }
}
