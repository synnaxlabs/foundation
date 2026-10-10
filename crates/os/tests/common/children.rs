//! Lists the sockets that a new child holds, for the test binaries of `os` that spawn
//! children.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env::net::Net;

const CHILDREN: usize = 500;

/// The sockets that a new child holds at the start of a test: those that this process
/// got from its parent and that are not closed on exec. Cargo leaves the socket of a
/// download open in each test binary that it then runs.
pub(crate) struct Baseline(Vec<String>);

impl Baseline {
    /// The sockets that a new child holds now. Call it before the first call of the
    /// test into `os`, so that no socket of `os` is in it.
    pub(crate) fn list() -> Self {
        Self(listed())
    }

    /// Each socket that a new child holds after its exec and that the baseline does
    /// not hold, by its path in `/dev/fd`.
    pub(crate) fn added(&self) -> Vec<String> {
        let mut added = listed();
        added.retain(|socket| !self.0.contains(socket));
        added
    }
}

/// Each socket that a new child holds after its exec, by its path in `/dev/fd`.
fn listed() -> Vec<String> {
    let list = r#"for f in /dev/fd/*; do if [ -S "$f" ]; then echo "$f"; fi; done"#;
    let listed = (Command::new("sh").args(["-c", list]).output()).expect("run sh");
    assert!(listed.status.success(), "sh lists the descriptors");
    let listed = String::from_utf8(listed.stdout).expect("sh writes UTF-8");
    listed.lines().map(String::from).collect()
}

/// Each socket that `baseline` does not hold and that each of 500 new children holds,
/// spawned while `threads` threads that `os` starts each run `work` in a loop.
pub(crate) fn held_while(
    baseline: &Baseline,
    threads: usize,
    work: impl AsyncFn(&Net) + Send + Sync + 'static,
) -> Vec<String> {
    let start = os::threads().expect("the OS gives the cores of this process");
    let stop = Arc::new(AtomicBool::new(false));
    let work = Arc::new(work);
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (stop, work) = (Arc::clone(&stop), Arc::clone(&work));
            let started = start.start("work", move || async move {
                let net = os::net();
                while !stop.load(Ordering::Relaxed) {
                    work(&net).await;
                }
            });
            started.expect("the thread starts")
        })
        .collect();
    let held = (0..CHILDREN).flat_map(|_| baseline.added()).collect();
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().expect("the work runs with no panic");
    }
    held
}
