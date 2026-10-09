//! A SIGINT and a SIGTERM that the process sends itself after `os::interrupt` end
//! no thread, and complete its future, which waits until then. This binary has no
//! test harness: the threads of a harness start before the hold, so a signal would
//! end the process.

use std::pin::pin;
use std::time::Duration;

use rustix::process::{Signal, getpid, kill_process};
use tokio::time::timeout;

/// How long the future must wait with no signal.
const QUIET: Duration = Duration::from_millis(200);
/// The bound of the wait for the signals.
const BOUND: Duration = Duration::from_secs(10);

fn main() {
    let interrupt = os::interrupt().expect("the signal thread starts");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let mut interrupt = pin!(interrupt);
        assert!(
            timeout(QUIET, interrupt.as_mut()).await.is_err(),
            "the future completed with no signal"
        );
        for signal in [Signal::INT, Signal::TERM] {
            kill_process(getpid(), signal).expect("the process signals itself");
        }
        timeout(BOUND, interrupt)
            .await
            .unwrap_or_else(|_| panic!("no signal came in {BOUND:?}"));
    });
}
