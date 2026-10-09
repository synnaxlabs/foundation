//! Starting shards: threads that each run a task executor, one per core.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::tasks::{Task, Tasks};
use crate::thread::{Error, Handle};

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
/// use env::thread::{Error, Handle};
///
/// fn start_all(shards: &env::shards::Shards) -> Result<Vec<Handle>, Error> {
///     (0..shards.cores().get())
///         .map(|core| {
///             let name = format!("shard-{core}");
///             let core = shards.pinnable().then_some(core);
///             let config = env::shards::Config { name, core };
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

    /// The number of cores this node may use. It never changes.
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

    /// Whether `start` can pin a shard to a core. It never changes. When `false`, set
    /// no [`Config::core`].
    ///
    /// ```
    /// fn core(shards: &env::shards::Shards, core: usize) -> Option<usize> {
    ///     shards.pinnable().then_some(core)
    /// }
    /// ```
    #[must_use]
    pub fn pinnable(&self) -> bool {
        self.0.pinnable()
    }

    /// Starts a shard. `main` runs on the new thread and gets the shard's [`Tasks`].
    /// It returns when the thread runs, is pinned, and has its executor.
    ///
    /// When the future that `main` returns completes, the shard drops its other tasks
    /// and the thread ends. A panic in `main`, in its future, or in a task of its
    /// [`Tasks`] ends the shard, and [`Handle::join`] returns
    /// [`Panicked`](crate::thread::Panicked). As anywhere in Rust, a panic that unwinds
    /// into the unwind of another panic aborts the process.
    ///
    /// # Errors
    ///
    /// - [`Error::Start`] when the thread or its executor cannot start.
    /// - [`Error::Pin`] when the thread cannot pin to `config.core`.
    ///
    /// # Panics
    ///
    /// When `config.core` is set and is not below [`Shards::cores`], or
    /// [`Shards::pinnable`] is `false`.
    ///
    /// ```
    /// use env::thread::{Error, Handle};
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
        if let Some(core) = config.core {
            let cores = self.cores();
            let name = &config.name;
            assert!(core < cores.get(), "{name} asks for core {core} of {cores}");
            let pinnable = self.pinnable();
            assert!(
                pinnable,
                "{name} asks for core {core} of a node that cannot pin"
            );
        }
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
    /// The core to pin the thread to, as an index below [`Shards::cores`] into the
    /// cores this node may use, never an OS CPU number; or `None` to let the OS place
    /// it. Set it only when [`Shards::pinnable`].
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
    /// The number of cores this node may use. It never changes.
    fn cores(&self) -> NonZeroUsize;

    /// Whether this driver can pin a shard to a core. It never changes.
    fn pinnable(&self) -> bool;

    /// Starts a thread with a task executor, makes [`Tasks`] for it, and runs
    /// `main(tasks)` on it, with the rules of [`Shards::start`]. `config.name` may hold
    /// any character; `os` gives the OS the part before the first NUL byte.
    /// `config.core`, when set, is below [`Driver::cores`] and the driver is
    /// pinnable: [`Shards::start`] checks both. Dropping the shard drops every task,
    /// also one that holds a clone of its [`Tasks`].
    ///
    /// # Errors
    ///
    /// - [`Error::Start`] when the thread or its executor cannot start.
    /// - [`Error::Pin`] when the thread cannot pin to `config.core`.
    fn start(&self, config: Config, main: Main) -> Result<Handle, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node with this many cores, whose shards end at once.
    struct Cores {
        count: NonZeroUsize,
        pinnable: bool,
    }

    impl Driver for Cores {
        fn cores(&self) -> NonZeroUsize {
            self.count
        }

        fn pinnable(&self) -> bool {
            self.pinnable
        }

        fn start(&self, _: Config, _: Main) -> Result<Handle, Error> {
            Ok(Handle::new(|| Ok(())))
        }
    }

    fn shards(cores: usize, pinnable: bool) -> Shards {
        let count = NonZeroUsize::new(cores).expect("a test asks for cores");
        Shards::new(Cores { count, pinnable })
    }

    fn start(shards: &Shards, core: Option<usize>) -> Result<(), Error> {
        let config = Config {
            name: "shard-4".into(),
            core,
        };
        let started = shards.start(config, |_| async {});
        started.map(|handle| handle.join().expect("a shard of Cores ends at once"))
    }

    #[test]
    fn each_core_below_the_count_starts() {
        for cores in 1..=8 {
            let shards = shards(cores, true);
            for core in 0..cores {
                assert_eq!(start(&shards, Some(core)), Ok(()), "{core} of {cores}");
            }
        }
    }

    #[test]
    #[should_panic(expected = "shard-4 asks for core 4 of 4")]
    fn the_core_at_the_count_panics() {
        start(&shards(4, true), Some(4)).unwrap();
    }

    #[test]
    #[should_panic(expected = "shard-4 asks for core 4 of 4")]
    fn the_core_at_the_count_panics_on_a_node_that_cannot_pin() {
        start(&shards(4, false), Some(4)).unwrap();
    }

    #[test]
    fn no_core_starts_on_any_count() {
        for pinnable in [true, false] {
            assert_eq!(start(&shards(1, pinnable), None), Ok(()));
            assert_eq!(start(&shards(8, pinnable), None), Ok(()));
        }
    }

    #[test]
    fn pinnable_is_the_drivers() {
        assert!(shards(4, true).pinnable());
        assert!(!shards(4, false).pinnable());
    }

    #[test]
    #[should_panic(expected = "shard-4 asks for core 2 of a node that cannot pin")]
    fn a_core_on_a_node_that_cannot_pin_panics() {
        start(&shards(4, false), Some(2)).unwrap();
    }
}
