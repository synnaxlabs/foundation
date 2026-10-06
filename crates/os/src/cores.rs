//! The cores a thread may run on, and placing a thread on them.

use std::io;
use std::num::NonZeroUsize;

use env::thread::Error;

/// The cores of the node.
#[derive(Debug)]
pub(crate) struct Cores {
    pub(crate) count: NonZeroUsize,
    /// The CPU of each core, or none where the OS cannot pin a thread.
    pub(crate) cpus: Vec<usize>,
}

impl Cores {
    /// Reads the cores of the calling thread. On Linux they are the CPUs of its
    /// affinity set, and the read fails with `EINVAL` on a kernel with more than 1024
    /// possible CPUs, the size of a `CpuSet`. Elsewhere only their count is known.
    pub(crate) fn read() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let (count, cpus) = {
            use rustix::thread::{CpuSet, sched_getaffinity};

            let set = sched_getaffinity(None)?;
            let cpus: Vec<_> = (0..CpuSet::MAX_CPU)
                .filter(|&cpu| set.is_set(cpu))
                .collect();
            let count = NonZeroUsize::new(cpus.len());
            (count.expect("a thread runs on a CPU of its set"), cpus)
        };
        #[cfg(not(target_os = "linux"))]
        #[expect(clippy::disallowed_methods, reason = "os reads the core count")]
        let (count, cpus) = (std::thread::available_parallelism()?, Vec::new());
        Ok(Self { count, cpus })
    }

    /// Places the calling thread, named `name`, on the CPU of `core`, an index below
    /// `count`, or on every CPU without one, so that it never keeps the CPUs of the
    /// thread that started it. Without CPUs, outside Linux, it does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Pin`] when the OS cannot pin it to `core`, and [`Error::Start`] when
    /// it cannot place it on every CPU.
    ///
    /// # Panics
    ///
    /// When `core` is set and the cores have no CPUs.
    #[cfg_attr(
        not(target_os = "linux"),
        expect(clippy::unnecessary_wraps, reason = "only Linux can fail to place")
    )]
    pub(crate) fn place(&self, name: &str, core: Option<usize>) -> Result<(), Error> {
        let cpus = match core {
            Some(core) => &self.cpus[core..=core],
            None => &self.cpus[..],
        };
        if cpus.is_empty() {
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        unreachable!("only Linux gives CPUs, here {cpus:?} for thread {name}");
        #[cfg(target_os = "linux")]
        {
            use rustix::thread::{CpuSet, sched_setaffinity};

            let mut set = CpuSet::new();
            for &cpu in cpus {
                set.set(cpu);
            }
            sched_setaffinity(None, &set).map_err(|e| {
                let (name, reason) = (name.to_owned(), io::Error::from(e).to_string());
                match core {
                    Some(core) => Error::Pin { name, core, reason },
                    None => Error::Start { name, reason },
                }
            })
        }
    }
}
