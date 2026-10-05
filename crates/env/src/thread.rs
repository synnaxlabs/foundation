//! A thread that `env` starts, from [`shards`](crate::shards) or
//! [`threads`](crate::threads): its handle, and why its start or its run failed.

use std::fmt;

/// A started thread. Join every handle: a dropped handle leaves its thread running,
/// and threads are never detached.
///
/// ```
/// fn stop(handle: env::thread::Handle) {
///     if let Err(e) = handle.join() {
///         panic!("{e}");
///     }
/// }
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// env::thread::Handle::new(|| Ok(()));
/// ```
#[must_use = "a dropped Handle leaves its thread running"]
pub struct Handle(Box<dyn FnOnce() -> Result<(), Error> + Send>);

impl Handle {
    /// Wraps the way a [`shards::Driver`](crate::shards::Driver) or a
    /// [`threads::Driver`](crate::threads::Driver) waits for its thread.
    ///
    /// ```
    /// let handle = env::thread::Handle::new(|| Ok(()));
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
    /// fn wait(handle: env::thread::Handle) -> Result<(), env::thread::Error> {
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
/// let e = env::thread::Error::Panicked { name: "shard-0".into() };
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

#[cfg(test)]
mod tests {
    use super::*;

    mod handle {
        use super::*;

        #[test]
        fn join_returns_the_outcome_of_the_driver() {
            let panicked = Error::Panicked {
                name: "shard-0".into(),
            };
            let outcome = panicked.clone();
            assert_eq!(Handle::new(move || Err(outcome)).join(), Err(panicked));
        }

        #[test]
        fn debug_hides_the_driver() {
            let handle = Handle::new(|| Ok(()));
            assert_eq!(format!("{handle:?}"), "Handle { .. }");
            handle.join().unwrap();
        }
    }

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
