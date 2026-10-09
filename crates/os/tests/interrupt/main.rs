//! A SIGINT and a SIGTERM that the process sends itself after `os::interrupt` end
//! no thread, and complete its future. This binary has no test harness: the threads
//! of a harness start before the hold, so a signal would end the process.

use std::time::Duration;

use rustix::process::{Signal, getpid, kill_process};

/// The bound of the wait.
const BOUND: Duration = Duration::from_secs(10);

fn main() {
    let interrupt = os::interrupt().expect("the signal thread starts");
    for signal in [Signal::INT, Signal::TERM] {
        kill_process(getpid(), signal).expect("the process signals itself");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime");
    runtime
        .block_on(async { tokio::time::timeout(BOUND, interrupt).await })
        .unwrap_or_else(|_| panic!("no signal came in {BOUND:?}"));
}
