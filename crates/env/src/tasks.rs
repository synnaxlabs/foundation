//! Spawning tasks on the current shard.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

/// A task as a driver receives it.
///
/// ```
/// let task: env::tasks::Task = Box::pin(async {});
/// ```
pub type Task = Pin<Box<dyn Future<Output = ()>>>;

/// Spawns tasks on one shard. Its tasks run only on that shard's thread, so they need
/// not be `Send`, and the handle cannot leave the thread. Clones spawn on the same
/// shard.
///
/// A shard's main future receives it from [`Threads::shard`](crate::Threads::shard).
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
///
/// fn count(tasks: &env::Tasks) -> Rc<Cell<u32>> {
///     let n = Rc::new(Cell::new(0));
///     let shared = Rc::clone(&n);
///     tasks.spawn(async move { shared.set(shared.get() + 1) });
///     n
/// }
/// ```
#[derive(Clone)]
pub struct Tasks(Rc<dyn Driver>);

impl Tasks {
    /// Wraps a driver.
    ///
    /// ```
    /// # struct Discard;
    /// # impl env::tasks::Driver for Discard {
    /// #     fn spawn(&self, _: env::tasks::Task) {}
    /// # }
    /// let tasks = env::Tasks::new(Discard);
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Rc::new(driver))
    }

    /// Starts `task` on this shard. It runs until its future completes; it has no
    /// handle. Stop it through its own cancel token, and send results back through a
    /// channel.
    ///
    /// ```
    /// fn start(tasks: &env::Tasks) {
    ///     tasks.spawn(async {});
    /// }
    /// ```
    pub fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        self.0.spawn(Box::pin(task));
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish_non_exhaustive()
    }
}

/// What `os` and `sim` implement to run [`Tasks`].
///
/// ```
/// use std::cell::RefCell;
///
/// /// Keeps tasks for a test to poll by hand.
/// struct Queue(RefCell<Vec<env::tasks::Task>>);
///
/// impl env::tasks::Driver for Queue {
///     fn spawn(&self, task: env::tasks::Task) {
///         self.0.borrow_mut().push(task);
///     }
/// }
/// ```
pub trait Driver {
    /// Starts `task` on this driver's shard.
    fn spawn(&self, task: Task);
}
