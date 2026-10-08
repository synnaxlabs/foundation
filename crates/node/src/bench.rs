//! `Scope`, for the bench target only. Not a stable surface.

use std::cell::Cell;
use std::rc::Rc;

use env::tasks::Task;

/// Futures that drop together, as `node` runs each session and stream. Dropped, it
/// drops each future that has not completed and wakes its task, which then ends.
#[expect(
    missing_debug_implementations,
    reason = "a bench surface over a scope with no `Debug`"
)]
pub struct Scope {
    scope: crate::scope::Scope,
    spawned: Rc<Cell<Option<Task>>>,
}

impl Scope {
    /// An empty scope whose tasks the caller polls by hand.
    #[must_use]
    pub fn new() -> Self {
        let spawned = Rc::default();
        let driver = Spawned(Rc::clone(&spawned));
        Self {
            scope: crate::scope::Scope::new(env::tasks::Tasks::new(driver)),
            spawned,
        }
    }

    /// Spawns `future` in the scope and gives the task that polls it.
    #[must_use = "the future runs only when the caller polls the task"]
    pub fn spawn(&mut self, future: Task) -> Task {
        self.scope.spawn(future);
        self.spawned
            .take()
            .expect("invariant: a spawn of the scope spawns one task")
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps the task of the last spawn for [`Scope::spawn`] to give.
struct Spawned(Rc<Cell<Option<Task>>>);

impl env::tasks::Driver for Spawned {
    fn spawn(&self, task: Task) {
        self.0.set(Some(task));
    }
}
