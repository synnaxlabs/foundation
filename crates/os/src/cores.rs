//! The cores this process may run on, and pinning a thread to one of them.

use std::io;
use std::num::NonZeroUsize;

/// The CPUs of the affinity set of this process, read once.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct Cores(Vec<usize>);

#[cfg(target_os = "linux")]
impl Cores {
    /// Reads the affinity set of the calling thread.
    pub(crate) fn read() -> io::Result<Self> {
        use rustix::thread::{CpuSet, sched_getaffinity};

        let set = sched_getaffinity(None)?;
        Ok(Self(
            (0..CpuSet::MAX_CPU)
                .filter(|&cpu| set.is_set(cpu))
                .collect(),
        ))
    }

    pub(crate) fn count(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.len()).expect("a thread runs on a CPU of its set")
    }

    /// Pins the calling thread to the CPU of `core`, an index below
    /// [`Cores::count`].
    pub(crate) fn pin(&self, core: usize) -> io::Result<()> {
        use rustix::thread::{CpuSet, sched_setaffinity};

        let mut set = CpuSet::new();
        set.set(self.0[core]);
        Ok(sched_setaffinity(None, &set)?)
    }
}

/// The count of CPUs that std gives.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct Cores(NonZeroUsize);

#[cfg(not(target_os = "linux"))]
impl Cores {
    #[expect(clippy::disallowed_methods, reason = "os reads the core count")]
    pub(crate) fn read() -> io::Result<Self> {
        std::thread::available_parallelism().map(Self)
    }

    pub(crate) fn count(&self) -> NonZeroUsize {
        self.0
    }

    /// Fails: this OS cannot pin a thread.
    #[expect(clippy::unused_self, reason = "the Linux form reads the set")]
    pub(crate) fn pin(&self, _: usize) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
