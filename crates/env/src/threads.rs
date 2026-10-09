//! Dedicated threads for blocking code, such as vendor libraries.

use std::fmt;
use std::sync::Arc;

use crate::tasks::Task;
use crate::thread::{Error, Handle};

/// A dedicated thread's body as a driver receives it.
///
/// ```
/// let body: env::threads::Body = Box::new(|| Box::pin(async {}));
/// ```
pub type Body = Box<dyn FnOnce() -> Task + Send>;

/// Starts dedicated threads. Clones start threads in the same place.
///
/// ```
/// use env::thread::{Error, Handle};
///
/// fn start(threads: &env::threads::Threads) -> Result<Handle, Error> {
///     threads.start("daqmx-dev1", || async {
///         // Call the blocking vendor library here, and await between calls.
///     })
/// }
/// ```
#[derive(Clone)]
pub struct Threads(Arc<dyn Driver>);

impl Threads {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::threads::Driver + 'static) -> env::threads::Threads {
    ///     env::threads::Threads::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Starts a thread named `name`. `body` runs on the new thread, and the thread
    /// runs the future it returns to completion, then ends. A panic in `body` or its
    /// future ends the thread, and [`Handle::join`] returns
    /// [`Panicked`](crate::thread::Panicked). As anywhere in Rust, a panic that unwinds
    /// into the unwind of another panic aborts the process.
    ///
    /// The future may block the thread, for example in a vendor call. To wait for an
    /// event, such as a value from a shard or a deadline, it awaits a future and never
    /// parks the thread itself, so that simulation controls every wait. It cannot
    /// spawn tasks.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread or its executor cannot start.
    ///
    /// ```
    /// use env::thread::{Error, Handle};
    ///
    /// fn start(threads: &env::threads::Threads) -> Result<Handle, Error> {
    ///     threads.start("modbus-poll", || async {})
    /// }
    /// ```
    pub fn start<F>(
        &self,
        name: &str,
        body: impl FnOnce() -> F + Send + 'static,
    ) -> Result<Handle, Error>
    where
        F: Future<Output = ()> + 'static,
    {
        self.0.start(name, Box::new(|| Box::pin(body())))
    }
}

impl fmt::Debug for Threads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Threads").finish_non_exhaustive()
    }
}

/// What `os` and `sim` implement to run [`Threads`]. Only they implement it.
///
/// ```
/// fn wrap(driver: impl env::threads::Driver + 'static) -> env::threads::Threads {
///     env::threads::Threads::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Starts a thread with an executor for one future, and runs `body()` on it to
    /// completion, with the rules of [`Threads::start`]. `name` may hold any
    /// character; `os` gives the OS the part before the first NUL byte.
    ///
    /// # Errors
    ///
    /// As [`Threads::start`].
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error>;
}
