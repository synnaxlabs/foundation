//! Dedicated threads for blocking code, such as vendor libraries, and the handle and
//! errors of every thread that `env` starts.

use std::fmt;
use std::sync::Arc;

use crate::tasks::Task;

/// A dedicated thread's body as a driver receives it.
///
/// ```
/// let body: env::threads::Body = Box::new(|| Box::pin(async {}));
/// ```
pub type Body = Box<dyn FnOnce() -> Task + Send>;

/// Starts dedicated threads. Clones start threads in the same place.
///
/// ```
/// use env::threads::{Error, Handle};
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
    /// runs the future it returns to completion, then ends.
    ///
    /// The future may block the thread, for example in a vendor call. To wait for an
    /// event, such as a value from a shard or a deadline, it awaits a future and never
    /// parks the thread itself, so that simulation controls every wait. It cannot
    /// spawn tasks.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread or its executor cannot start, or when `name`
    /// holds a NUL byte.
    ///
    /// ```
    /// use env::threads::{Error, Handle};
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

/// A started thread. Join every handle: a dropped handle leaves its thread running,
/// and threads are never detached.
///
/// ```
/// fn stop(handle: env::threads::Handle) {
///     if let Err(e) = handle.join() {
///         panic!("{e}");
///     }
/// }
/// ```
#[must_use = "a dropped Handle leaves its thread running"]
pub struct Handle(Box<dyn FnOnce() -> Result<(), Error> + Send>);

impl Handle {
    /// Wraps the driver's way to wait for the thread.
    ///
    /// ```
    /// let handle = env::threads::Handle::new(|| Ok(()));
    /// assert_eq!(handle.join(), Ok(()));
    /// ```
    pub fn new(join: impl FnOnce() -> Result<(), Error> + Send + 'static) -> Self {
        Self(Box::new(join))
    }

    /// Blocks until the thread ends. Call it only on a thread that `env` did not
    /// start, such as `node`'s main thread.
    ///
    /// # Errors
    ///
    /// [`Error::Panicked`] when the thread or one of its tasks panicked.
    ///
    /// ```
    /// fn wait(handle: env::threads::Handle) -> Result<(), env::threads::Error> {
    ///     handle.join()
    /// }
    /// ```
    pub fn join(self) -> Result<(), Error> {
        (self.0)()
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

/// Why a thread failed.
///
/// ```
/// let e = env::threads::Error::Panicked { name: "shard-0".into() };
/// assert_eq!(e.to_string(), "thread shard-0 panicked");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The thread or its executor could not start.
    Start {
        /// The thread's name.
        name: String,
        /// What the OS or the simulation reported.
        reason: String,
    },
    /// A shard could not pin its thread to a core.
    Pin {
        /// The thread's name.
        name: String,
        /// The core it asked for.
        core: usize,
    },
    /// The thread or one of its tasks panicked.
    Panicked {
        /// The thread's name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start { name, reason } => {
                write!(f, "cannot start thread {name}: {reason}")
            }
            Self::Pin { name, core } => {
                write!(f, "cannot pin thread {name} to core {core}")
            }
            Self::Panicked { name } => write!(f, "thread {name} panicked"),
        }
    }
}

impl std::error::Error for Error {}

/// What `os` and `sim` implement to run [`Threads`]. Only they implement it.
///
/// ```
/// fn wrap(driver: impl env::threads::Driver + 'static) -> env::threads::Threads {
///     env::threads::Threads::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Starts a thread with an executor for one future, and runs `body()` on it to
    /// completion, with the rules of [`Threads::start`].
    ///
    /// # Errors
    ///
    /// As [`Threads::start`].
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    mod error {
        use super::*;

        #[test]
        fn names_the_thread_and_the_reason_when_it_cannot_start() {
            let e = Error::Start {
                name: "shard-3".into(),
                reason: "no core 3".into(),
            };
            assert_eq!(e.to_string(), "cannot start thread shard-3: no core 3");
        }

        #[test]
        fn names_the_thread_and_the_core_when_it_cannot_pin() {
            let e = Error::Pin {
                name: "shard-3".into(),
                core: 3,
            };
            assert_eq!(e.to_string(), "cannot pin thread shard-3 to core 3");
        }

        #[test]
        fn names_the_thread_that_panicked() {
            let e = Error::Panicked {
                name: "shard-3".into(),
            };
            assert_eq!(e.to_string(), "thread shard-3 panicked");
        }
    }
}
