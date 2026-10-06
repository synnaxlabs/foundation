//! Helpers for the tests of shards and dedicated threads.

use std::sync::mpsc;
use std::time::Duration;

use env::thread::{Handle, Panicked};

/// Asserts that `handle` joins with `outcome` in ten seconds, so a thread that does not
/// end fails its test and does not hang the run.
#[expect(clippy::disallowed_methods, reason = "the test bounds the join")]
pub(crate) fn assert_joins(handle: Handle, outcome: Result<(), Panicked>) {
    let (done, joined) = mpsc::channel();
    std::thread::spawn(move || done.send(handle.join()));
    let joined = joined.recv_timeout(Duration::from_secs(10));
    assert_eq!(joined, Ok(outcome), "the thread ends in ten seconds");
}

pub(crate) fn panicked(name: &str) -> Result<(), Panicked> {
    Err(Panicked { name: name.into() })
}

/// Panics when it drops.
pub(crate) struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("bomb");
    }
}

/// The CPUs of the affinity set of the calling thread.
#[cfg(target_os = "linux")]
pub(crate) fn affinity() -> Vec<usize> {
    use rustix::thread::{CpuSet, sched_getaffinity};

    let set = sched_getaffinity(None).unwrap();
    (0..CpuSet::MAX_CPU)
        .filter(|&cpu| set.is_set(cpu))
        .collect()
}
