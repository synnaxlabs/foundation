//! The cores a thread may run on, and pinning a thread to one of them.

use std::io;
use std::num::NonZeroUsize;

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

    /// Pins the calling thread to the CPU of `core`, an index below `count`.
    ///
    /// # Panics
    ///
    /// When the cores have no CPUs.
    pub(crate) fn pin(&self, core: usize) -> io::Result<()> {
        let cpu = self.cpus[core];
        #[cfg(not(target_os = "linux"))]
        unreachable!("only Linux gives the CPU of a core, here {cpu}");
        #[cfg(target_os = "linux")]
        {
            use rustix::thread::{CpuSet, sched_setaffinity};

            let mut set = CpuSet::new();
            set.set(cpu);
            Ok(sched_setaffinity(None, &set)?)
        }
    }
}
