//! Lists the sockets that a new child holds, for the test binaries of `os` that spawn
//! children.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env::net::Net;

const CHILDREN: usize = 500;

/// The sockets that a new child holds before a test opens any: those that this process
/// got from its parent and that are not closed on exec. Cargo leaves the socket of a
/// download open in each test binary that it then runs.
pub(crate) struct Inherited(Vec<String>);

impl Inherited {
    /// The sockets that a new child holds now.
    pub(crate) fn list() -> Self {
        Self(listed())
    }

    /// Each socket that a new child holds after its exec and that is not inherited, by
    /// its path in `/dev/fd`.
    pub(crate) fn held(&self) -> Vec<String> {
        let mut held = listed();
        held.retain(|socket| !self.0.contains(socket));
        held
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

/// Each socket that is not inherited and that each of 500 new children holds, spawned
/// while `threads` threads that `os` starts each run `work` in a loop.
pub(crate) fn held_while(
    threads: usize,
    work: impl AsyncFn(&Net) + Send + Sync + 'static,
) -> Vec<String> {
    let inherited = Inherited::list();
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
    let held = (0..CHILDREN).flat_map(|_| inherited.held()).collect();
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().expect("the work runs with no panic");
    }
    held
}
