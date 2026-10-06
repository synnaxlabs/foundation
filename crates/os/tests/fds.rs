//! Sleeps while the process has no free fd. It sets a limit on the whole process, so
//! it runs in a test binary of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::fd::AsRawFd;

use rustix::io::Errno;
use rustix::process::{self, Resource, Rlimit};
use types::time::Span;

/// Lets the process open no new fd, and returns the limit it had.
fn take_every_fd() -> Rlimit {
    let limit = process::getrlimit(Resource::Nofile);
    let lowest = rustix::io::dup(std::io::stderr()).unwrap().as_raw_fd();
    let none = Rlimit {
        current: Some(u64::try_from(lowest).unwrap()),
        maximum: limit.maximum,
    };
    process::setrlimit(Resource::Nofile, none).unwrap();
    assert_eq!(
        rustix::io::dup(std::io::stderr()).unwrap_err(),
        Errno::MFILE
    );
    limit
}

#[test]
fn a_sleep_completes_when_the_process_has_no_free_fd() {
    let clock = os::clock();
    let threads = os::threads().expect("the OS gives the cores of this process");
    let handle = threads.start("sleeper", move || async move {
        let limit = take_every_fd();
        for _ in 0..20 {
            let deadline = clock.now() + Span::from_nanos(1_000_000);
            clock.sleep_until(deadline).await;
            assert!(clock.now() >= deadline, "early: {:?}", clock.now());
        }
        process::setrlimit(Resource::Nofile, limit).unwrap();
    });
    handle.unwrap().join().unwrap();
}
