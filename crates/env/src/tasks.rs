//! Spawning tasks on the current shard.

use std::fmt;
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
/// A shard's main function receives it from
/// [`Shards::start`](crate::shards::Shards::start).
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
///
/// fn count(tasks: &env::tasks::Tasks) -> Rc<Cell<u32>> {
///     let n = Rc::new(Cell::new(0));
///     let shared = Rc::clone(&n);
///     tasks.spawn(async move { shared.set(shared.get() + 1) });
///     n
/// }
/// ```
#[derive(Clone)]
pub struct Tasks(Rc<dyn Driver>);

impl Tasks {
    /// Wraps `driver`.
    ///
    /// ```
    /// fn wrap(driver: impl env::tasks::Driver + 'static) -> env::tasks::Tasks {
    ///     env::tasks::Tasks::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Rc::new(driver))
    }

    /// Starts `task` on this shard. It has no handle: stop it through its own cancel
    /// token, and send results back through a channel.
    ///
    /// It runs until its future completes or the shard's main future completes,
    /// which drops it. A panic in it ends the shard (see
    /// [`Shards::start`](crate::shards::Shards::start)).
    ///
    /// ```
    /// fn start(tasks: &env::tasks::Tasks) {
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

/// What [`Tasks`] runs its tasks on. `os` and `sim` implement it for a shard. A driver
/// that counts tasks passes each task to one of theirs. A driver of a test or a
/// benchmark may keep each task for its caller to poll.
///
/// ```
/// fn wrap(driver: impl env::tasks::Driver + 'static) -> env::tasks::Tasks {
///     env::tasks::Tasks::new(driver)
/// }
/// ```
pub trait Driver {
    /// Starts `task` on this driver's shard, with the rules of [`Tasks::spawn`].
    fn spawn(&self, task: Task);
}
