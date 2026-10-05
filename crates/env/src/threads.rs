//! Starting threads: shards that run tasks, and dedicated threads for blocking code.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::Tasks;
use crate::tasks::Task;

/// A shard's main function as a driver receives it.
///
/// ```
/// let main: env::threads::Main = Box::new(|_tasks| Box::pin(async {}));
/// ```
pub type Main = Box<dyn FnOnce(Tasks) -> Task + Send>;

/// A dedicated thread's body as a driver receives it.
///
/// ```
/// let body: env::threads::Body = Box::new(|| {});
/// ```
pub type Body = Box<dyn FnOnce() + Send>;

/// Starts threads. Clones start threads in the same place.
///
/// ```
/// use env::threads::{Config, Error, Handle};
///
/// fn start(threads: &env::Threads) -> Result<Handle, Error> {
///     let config = Config { name: "shard-0".into(), core: Some(0) };
///     threads.shard(config, |tasks| async move {
///         tasks.spawn(async {});
///     })
/// }
/// ```
#[derive(Clone)]
pub struct Threads(Arc<dyn Driver>);

impl Threads {
    /// Wraps a driver.
    ///
    /// ```
    /// # use env::threads::{Body, Config, Error, Handle, Main};
    /// # struct Refuse;
    /// # impl env::threads::Driver for Refuse {
    /// #     fn shard(&self, c: Config, _: Main) -> Result<Handle, Error> {
    /// #         Err(Error::Start { name: c.name, reason: "refused".into() })
    /// #     }
    /// #     fn dedicated(&self, c: Config, _: Body) -> Result<Handle, Error> {
    /// #         Err(Error::Start { name: c.name, reason: "refused".into() })
    /// #     }
    /// # }
    /// let threads = env::Threads::new(Refuse);
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Starts a shard: a thread with its own task executor. `main` runs on the new
    /// thread and gets the shard's [`Tasks`]. The thread ends when the future that
    /// `main` returns completes.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread cannot start.
    ///
    /// ```
    /// use env::threads::{Config, Error, Handle};
    ///
    /// fn start(threads: &env::Threads) -> Result<Handle, Error> {
    ///     let config = Config { name: "shard-1".into(), core: Some(1) };
    ///     threads.shard(config, |_tasks| async {})
    /// }
    /// ```
    pub fn shard<F>(
        &self,
        config: Config,
        main: impl FnOnce(Tasks) -> F + Send + 'static,
    ) -> Result<Handle, Error>
    where
        F: Future<Output = ()> + 'static,
    {
        self.0
            .shard(config, Box::new(|tasks| Box::pin(main(tasks))))
    }

    /// Starts a dedicated thread for blocking code, such as a vendor library. The
    /// thread ends when `body` returns.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread cannot start.
    ///
    /// ```
    /// use env::threads::{Config, Error, Handle};
    ///
    /// fn start(threads: &env::Threads) -> Result<Handle, Error> {
    ///     let config = Config { name: "daqmx-dev1".into(), core: None };
    ///     threads.dedicated(config, || {})
    /// }
    /// ```
    pub fn dedicated(
        &self,
        config: Config,
        body: impl FnOnce() + Send + 'static,
    ) -> Result<Handle, Error> {
        self.0.dedicated(config, Box::new(body))
    }
}

impl fmt::Debug for Threads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Threads").finish_non_exhaustive()
    }
}

/// Settings for one thread.
///
/// ```
/// let config = env::threads::Config { name: "shard-2".into(), core: Some(2) };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The thread's name, shown by the OS and in errors.
    pub name: String,
    /// The core to pin the thread to, or `None` to let the OS place it.
    pub core: Option<usize>,
}

/// A started thread. Dropping it leaves the thread running.
///
/// ```
/// fn stop(handle: env::threads::Handle) {
///     if let Err(e) = handle.join() {
///         panic!("{e}");
///     }
/// }
/// ```
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

    /// Blocks until the thread ends. Never call it on a shard.
    ///
    /// # Errors
    ///
    /// [`Error::Panicked`] when the thread panicked.
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
    /// The thread could not start.
    Start {
        /// The thread's name.
        name: String,
        /// What the OS or the simulation reported.
        reason: String,
    },
    /// The thread panicked.
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
            Self::Panicked { name } => write!(f, "thread {name} panicked"),
        }
    }
}

impl std::error::Error for Error {}

/// What `os` and `sim` implement to run [`Threads`].
///
/// ```
/// use env::threads::{Body, Config, Error, Handle, Main};
///
/// /// Refuses every thread, for a node that must not start any.
/// struct Refuse;
///
/// impl env::threads::Driver for Refuse {
///     fn shard(&self, config: Config, _: Main) -> Result<Handle, Error> {
///         Err(Error::Start { name: config.name, reason: "refused".into() })
///     }
///     fn dedicated(&self, config: Config, _: Body) -> Result<Handle, Error> {
///         Err(Error::Start { name: config.name, reason: "refused".into() })
///     }
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Starts a thread that runs a task executor, makes [`Tasks`] for it, and runs
    /// `main(tasks)` on it until that future completes.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread cannot start.
    fn shard(&self, config: Config, main: Main) -> Result<Handle, Error>;

    /// Starts a thread that runs `body`.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread cannot start.
    fn dedicated(&self, config: Config, body: Body) -> Result<Handle, Error>;
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
        fn names_the_thread_that_panicked() {
            let e = Error::Panicked {
                name: "shard-3".into(),
            };
            assert_eq!(e.to_string(), "thread shard-3 panicked");
        }
    }
}
