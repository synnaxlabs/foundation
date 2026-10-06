//! The start and the join of an OS thread, for a shard or a dedicated thread.

use std::panic;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use env::thread::{Error, Handle, Panicked};

use crate::unwind;

/// Starts OS thread `name`, whose OS name is the part of `name` before the first NUL.
/// On it, `build` makes the runtime of the thread and `serve` runs it, returning
/// whether the thread panicked. Blocks until `build` returns, and gives its error.
/// The join of the handle gives [`Panicked`] when `serve` returns `true` or panics.
#[expect(
    clippy::disallowed_methods,
    reason = "os starts threads, and a start blocks until its thread runs"
)]
pub(crate) fn start<R>(
    name: String,
    build: impl FnOnce(&str) -> Result<R, Error> + Send + 'static,
    serve: impl FnOnce(R) -> bool + Send + 'static,
) -> Result<Handle, Error> {
    let (report, started) = mpsc::sync_channel(1);
    let own = name.clone();
    let os_name = name
        .split_once('\0')
        .map_or(name.as_str(), |(head, _)| head);
    let thread = thread::Builder::new()
        .name(os_name.to_owned())
        .spawn(move || run(&own, build, serve, &report))
        .map_err(|e| Error::Start {
            name: name.clone(),
            reason: e.to_string(),
        })?;
    match started.recv() {
        Ok(Ok(())) => Ok(Handle::new(move || {
            match thread.join().map_err(unwind::discard) {
                Ok(false) => Ok(()),
                Ok(true) | Err(()) => Err(Panicked { name }),
            }
        })),
        // The thread holds nothing after its report, and ends.
        Ok(Err(e)) => Err(e),
        Err(_) => panic::resume_unwind(
            thread
                .join()
                .expect_err("a thread that did not report panicked"),
        ),
    }
}

/// Builds, reports the start to `start`, and serves. Returns whether the thread
/// panicked.
fn run<R>(
    name: &str,
    build: impl FnOnce(&str) -> Result<R, Error>,
    serve: impl FnOnce(R) -> bool,
    report: &SyncSender<Result<(), Error>>,
) -> bool {
    match build(name) {
        Ok(runtime) => {
            report.send(Ok(())).expect("start waits for the report");
            serve(runtime)
        }
        Err(e) => {
            // First, so that a panic in its drop reaches `start` as no report.
            drop(serve);
            report.send(Err(e)).expect("start waits for the report");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::unwind::tests::Relay;

    #[test]
    fn a_serve_that_panics_with_a_payload_that_panics_in_its_drop_gives_panicked() {
        let drops = Arc::new(AtomicUsize::new(0));
        let relay = Relay {
            left: 2,
            drops: Arc::clone(&drops),
        };
        let serve = move |()| -> bool { panic::panic_any(relay) };
        let handle = start("relay".to_owned(), |_| Ok(()), serve).unwrap();
        let name = "relay".to_owned();
        assert_eq!(handle.join(), Err(Panicked { name }));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }
}
