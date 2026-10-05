//! Starting shards: threads that each run a task executor, one per core.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::tasks::{Task, Tasks};
use crate::threads::{self, Error, Handle};

/// A shard's main function as a driver receives it.
///
/// ```
/// let main: env::shards::Main = Box::new(|_tasks| Box::pin(async {}));
/// ```
pub type Main = Box<dyn FnOnce(Tasks) -> Task + Send>;

/// Starts shards. Only `node` holds it, so only `node` decides where shards run.
/// Clones start shards in the same place.
///
/// ```
/// use env::threads::{Error, Handle};
///
/// fn start_all(shards: &env::shards::Shards) -> Result<Vec<Handle>, Error> {
///     (0..shards.cores().get())
///         .map(|core| {
///             let name = format!("shard-{core}");
///             let config = env::shards::Config { name, core: Some(core) };
///             shards.start(config, |tasks| async move {
///                 tasks.spawn(async {});
///             })
///         })
///         .collect()
/// }
/// ```
#[derive(Clone)]
pub struct Shards(Arc<dyn Driver>);

impl Shards {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::shards::Driver + 'static) -> env::shards::Shards {
    ///     env::shards::Shards::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// The number of cores this node may use.
    ///
    /// ```
    /// fn count(shards: &env::shards::Shards) -> usize {
    ///     shards.cores().get()
    /// }
    /// ```
    #[must_use]
    pub fn cores(&self) -> NonZeroUsize {
        self.0.cores()
    }

    /// Starts a shard. `main` runs on the new thread and gets the shard's [`Tasks`].
    /// It returns when the thread runs, is pinned, and has its executor.
    ///
    /// When the future that `main` returns completes, the shard drops its other tasks
    /// and the thread ends. A panic in any of its tasks ends the shard, and
    /// [`Handle::join`] returns [`Error::Panicked`].
    ///
    /// # Errors
    ///
    /// - [`Error::Start`] when the thread or its executor cannot start, or when the
    ///   name holds a NUL byte.
    /// - [`Error::Pin`] when the thread cannot pin to `config.core`.
    ///
    /// ```
    /// use env::threads::{Error, Handle};
    ///
    /// fn start(shards: &env::shards::Shards) -> Result<Handle, Error> {
    ///     let config = env::shards::Config { name: "shard-1".into(), core: None };
    ///     shards.start(config, |_tasks| async {})
    /// }
    /// ```
    pub fn start<F>(
        &self,
        config: Config,
        main: impl FnOnce(Tasks) -> F + Send + 'static,
    ) -> Result<Handle, Error>
    where
        F: Future<Output = ()> + 'static,
    {
        threads::check_name(&config.name)?;
        self.0
            .start(config, Box::new(|tasks| Box::pin(main(tasks))))
    }
}

impl fmt::Debug for Shards {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shards").finish_non_exhaustive()
    }
}

/// Settings for one shard.
///
/// ```
/// let config = env::shards::Config { name: "shard-2".into(), core: Some(2) };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The thread's name, shown by the OS and in errors.
    pub name: String,
    /// The core to pin the thread to, or `None` to let the OS place it.
    pub core: Option<usize>,
}

/// What `os` and `sim` implement to run [`Shards`]. Only they implement it.
///
/// ```
/// fn wrap(driver: impl env::shards::Driver + 'static) -> env::shards::Shards {
///     env::shards::Shards::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// The number of cores this node may use.
    fn cores(&self) -> NonZeroUsize;

    /// Starts a thread with a task executor, makes [`Tasks`] for it, and runs
    /// `main(tasks)` on it, with the rules of [`Shards::start`]. `config.name` holds no
    /// NUL byte. Dropping the shard drops every task, also one that holds a clone of
    /// its [`Tasks`].
    ///
    /// # Errors
    ///
    /// As [`Shards::start`].
    fn start(&self, config: Config, main: Main) -> Result<Handle, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that starts every shard at once.
    struct Started;

    impl Driver for Started {
        fn cores(&self) -> NonZeroUsize {
            NonZeroUsize::MIN
        }

        fn start(&self, _: Config, _: Main) -> Result<Handle, Error> {
            Ok(Handle::new(|| Ok(())))
        }
    }

    fn start(name: &str) -> Result<Handle, Error> {
        let config = Config {
            name: name.into(),
            core: None,
        };
        Shards::new(Started).start(config, |_tasks| async {})
    }

    #[test]
    fn a_name_without_a_nul_byte_reaches_the_driver() {
        let Ok(handle) = start("shard-0") else {
            panic!("shard-0 did not start");
        };
        assert_eq!(handle.join(), Ok(()));
    }

    #[test]
    fn a_name_with_a_nul_byte_cannot_start() {
        let Err(e) = start("shard-0\0") else {
            panic!("a name with a NUL byte started");
        };
        let reason = "the name holds a NUL byte".into();
        assert_eq!(
            e,
            Error::Start {
                name: "shard-0\0".into(),
                reason
            }
        );
    }
}
