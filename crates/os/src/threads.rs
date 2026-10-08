//! Dedicated threads on OS threads, each with a current-thread Tokio runtime.

use std::sync::Arc;
use std::task::{Context, Wake, Waker};

use env::thread::{Error, Handle};
use env::threads::Body;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;

use crate::cores::Cores;
use crate::{thread, unwind};

/// Starts each dedicated thread on its own OS thread.
pub(crate) struct Driver(Arc<Cores>);

impl Driver {
    pub(crate) fn new(cores: Cores) -> Self {
        Self(Arc::new(cores))
    }
}

impl env::threads::Driver for Driver {
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error> {
        let cores = Arc::clone(&self.0);
        let build = move |name: &str| build(&cores, name);
        thread::start(name.to_owned(), build, |runtime| serve(runtime, body))
    }
}

/// Places the calling thread on every CPU of the node and builds its runtime.
fn build(cores: &Cores, name: &str) -> Result<Runtime, Error> {
    cores.place(name, None)?;
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| Error::Start {
            name: name.to_owned(),
            reason: e.to_string(),
        })
}

/// Runs `body` to completion, then drops `runtime`. Returns whether a panic happened,
/// in the call, the poll, or the drop of the body, or in the drop of `runtime`. Tokio
/// catches a panic in the drop of a task that the body spawned on `runtime`, but not
/// each panic in the drops of its payloads.
fn serve(runtime: Runtime, body: Body) -> bool {
    let panicked = drive(&runtime, body);
    unwind::catch(|| drop(runtime)).is_none() || panicked
}

/// Runs `body` to completion. Returns whether it panicked, in its call, its poll, or
/// its drop.
///
/// The body runs in the context of `runtime` but outside its `block_on`, so that a
/// vendor call in it may start and block on a runtime of its own. Between polls the
/// thread waits in `block_on`, which drives the timers of `runtime`.
fn drive(runtime: &Runtime, body: Body) -> bool {
    let signal = Arc::new(Signal(Notify::new()));
    let waker = Waker::from(Arc::clone(&signal));
    let mut cx = Context::from_waker(&waker);
    let _entered = runtime.enter();
    let Some(mut task) = unwind::catch(body) else {
        return true;
    };
    let polled = unwind::catch(|| {
        while task.as_mut().poll(&mut cx).is_pending() {
            runtime.block_on(signal.0.notified());
        }
    });
    // Apart from the poll, so that a panic in the drop does not abort the unwind of a
    // panic in the poll.
    let dropped = unwind::catch(|| drop(task));
    polled.is_none() || dropped.is_none()
}

/// The waker of a body: it ends the wait of its thread, from any thread.
struct Signal(Notify);

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
}
