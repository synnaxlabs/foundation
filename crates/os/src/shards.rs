//! Shards on OS threads, each with a Tokio `LocalRuntime`.

use std::cell::Cell;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::task::{Context, Poll, Waker};
use std::thread;

use env::shards::{Config, Main};
use env::tasks::{Task, Tasks};
use env::thread::{Error, Handle, Panicked};
use tokio::runtime::{Builder, LocalOptions, LocalRuntime};

use crate::cores::Cores;

/// Starts each shard on its own thread.
pub(crate) struct Driver(Arc<Cores>);

impl Driver {
    pub(crate) fn new(cores: Arc<Cores>) -> Self {
        Self(cores)
    }
}

impl env::shards::Driver for Driver {
    fn cores(&self) -> NonZeroUsize {
        self.0.count()
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "os starts threads, and a start blocks until its thread runs"
    )]
    fn start(&self, config: Config, main: Main) -> Result<Handle, Error> {
        let Config { name, core } = config;
        let pin = core.map(|core| (Arc::clone(&self.0), core));
        let (report, started) = mpsc::sync_channel(1);
        let shard = name.clone();
        let os = name
            .split_once('\0')
            .map_or(name.as_str(), |(head, _)| head);
        let thread = thread::Builder::new()
            .name(os.to_owned())
            .spawn(move || run(&shard, pin, main, &report))
            .map_err(|e| Error::Start {
                name: name.clone(),
                reason: e.to_string(),
            })?;
        if started.recv().is_ok() {
            return Ok(Handle::new(move || match thread.join() {
                Ok(Ok(false)) => Ok(()),
                // `Ok(Err(_))` cannot come after the report.
                _ => Err(Panicked { name }),
            }));
        }
        match thread.join() {
            Ok(outcome) => {
                Err(outcome.expect_err("a shard that did not report failed"))
            }
            Err(payload) => panic::resume_unwind(payload),
        }
    }
}

/// Runs one shard on its thread: pins it, builds its runtime, and serves it. Returns
/// whether a task panicked, or why the shard could not start.
fn run(
    name: &str,
    pin: Option<(Arc<Cores>, usize)>,
    main: Main,
    report: &SyncSender<()>,
) -> Result<bool, Error> {
    if let Some((cores, core)) = pin
        && cores.pin(core).is_err()
    {
        let name = name.to_owned();
        return Err(Error::Pin { name, core });
    }
    let runtime = Builder::new_current_thread()
        .build_local(LocalOptions::default())
        .map_err(|e| Error::Start {
            name: name.to_owned(),
            reason: e.to_string(),
        })?;
    Ok(serve(runtime, main, report))
}

/// Reports the start, runs `main` until it ends or a task panics, then drops every
/// task. Returns whether a task panicked.
fn serve(runtime: LocalRuntime, main: Main, report: &SyncSender<()>) -> bool {
    report.send(()).expect("start waits for the report");
    let shard = Rc::new(Shard::default());
    let tasks = Tasks::new(Spawner(Rc::clone(&shard)));
    runtime.block_on(async {
        let main = Box::pin(async move { main(tasks).await });
        let mut main = Caught::new(main, Rc::clone(&shard));
        poll_fn(|cx| {
            let mut waker = shard.waker.replace(Waker::noop().clone());
            waker.clone_from(cx.waker());
            shard.waker.set(waker);
            if shard.panicked.get() {
                return Poll::Ready(());
            }
            Pin::new(&mut main).poll(cx)
        })
        .await;
    });
    // The drop of a task may panic.
    drop(runtime);
    shard.panicked.get()
}

/// What the tasks of one shard share with its main loop.
struct Shard {
    /// Whether a task panicked, in its poll or its drop.
    panicked: Cell<bool>,
    /// Wakes the main loop.
    waker: Cell<Waker>,
}

impl Default for Shard {
    fn default() -> Self {
        Self {
            panicked: Cell::new(false),
            waker: Cell::new(Waker::noop().clone()),
        }
    }
}

impl Shard {
    fn panic(&self) {
        self.panicked.set(true);
        self.waker.replace(Waker::noop().clone()).wake();
    }
}

/// Spawns tasks on the runtime of the calling thread, the shard's own.
struct Spawner(Rc<Shard>);

impl env::tasks::Driver for Spawner {
    fn spawn(&self, task: Task) {
        drop(tokio::task::spawn_local(Caught::new(
            task,
            Rc::clone(&self.0),
        )));
    }
}

/// A task whose panics, in its poll or its drop, end its shard. Tokio would drop the
/// panic of a spawned task and miss one in its drop.
struct Caught {
    task: Option<Task>,
    shard: Rc<Shard>,
}

impl Caught {
    fn new(task: Task, shard: Rc<Shard>) -> Self {
        Self {
            task: Some(task),
            shard,
        }
    }
}

impl Future for Caught {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let Some(task) = self.task.as_mut() else {
            return Poll::Ready(());
        };
        let poll = panic::catch_unwind(AssertUnwindSafe(|| task.as_mut().poll(cx)));
        poll.unwrap_or_else(|_| {
            self.shard.panic();
            Poll::Ready(())
        })
    }
}

impl Drop for Caught {
    fn drop(&mut self) {
        let task = self.task.take();
        if panic::catch_unwind(AssertUnwindSafe(|| drop(task))).is_err() {
            self.shard.panic();
        }
    }
}
