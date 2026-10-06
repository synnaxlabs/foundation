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
    pub(crate) fn new(cores: Cores) -> Self {
        Self(Arc::new(cores))
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
        let thread_name = name
            .split_once('\0')
            .map_or(name.as_str(), |(head, _)| head);
        let thread = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || run(&shard, pin, main, &report))
            .map_err(|e| Error::Start {
                name: name.clone(),
                reason: e.to_string(),
            })?;
        match started.recv() {
            Ok(Ok(())) => Ok(Handle::new(move || match thread.join() {
                Ok(false) => Ok(()),
                Ok(true) | Err(_) => Err(Panicked { name }),
            })),
            // The thread holds nothing after its report, and ends.
            Ok(Err(e)) => Err(e),
            Err(_) => panic::resume_unwind(
                thread
                    .join()
                    .expect_err("a shard that did not report panicked"),
            ),
        }
    }
}

/// Runs one shard on its thread: builds it, reports the start to `start`, and serves
/// it. Returns whether a task panicked.
fn run(
    name: &str,
    pin: Option<(Arc<Cores>, usize)>,
    main: Main,
    report: &SyncSender<Result<(), Error>>,
) -> bool {
    let runtime = match build(name, pin) {
        Ok(runtime) => runtime,
        Err(e) => {
            // First, so that a panic in its drop reaches `start` as no report.
            drop(main);
            report.send(Err(e)).expect("start waits for the report");
            return false;
        }
    };
    report.send(Ok(())).expect("start waits for the report");
    serve(runtime, main)
}

/// Pins the calling thread and builds its runtime.
fn build(name: &str, pin: Option<(Arc<Cores>, usize)>) -> Result<LocalRuntime, Error> {
    if let Some((cores, core)) = pin
        && cores.pin(core).is_err()
    {
        let name = name.to_owned();
        return Err(Error::Pin { name, core });
    }
    Builder::new_current_thread()
        .build_local(LocalOptions::default())
        .map_err(|e| Error::Start {
            name: name.to_owned(),
            reason: e.to_string(),
        })
}

/// Runs `main` until it ends or a task panics, then drops every task. Returns whether a
/// task panicked.
fn serve(runtime: LocalRuntime, main: Main) -> bool {
    let alarm = Rc::new(Alarm::default());
    let tasks = Tasks::new(Spawner(Rc::clone(&alarm)));
    runtime.block_on(async {
        // Calls `main` inside the catch.
        let main = Box::pin(async move { main(tasks).await });
        let mut main = Caught::new(main, Rc::clone(&alarm));
        poll_fn(|cx| {
            // `clone_from` skips the clone when the waker is the same.
            let mut waker = alarm.waker.replace(Waker::noop().clone());
            waker.clone_from(cx.waker());
            alarm.waker.set(waker);
            if alarm.raised.get() {
                return Poll::Ready(());
            }
            Pin::new(&mut main).poll(cx)
        })
        .await;
    });
    // The drop of a task may panic.
    drop(runtime);
    alarm.raised.get()
}

/// Tells the main loop of a shard that a task panicked.
struct Alarm {
    /// Whether a task panicked, in its poll or its drop.
    raised: Cell<bool>,
    /// Wakes the main loop.
    waker: Cell<Waker>,
}

impl Default for Alarm {
    fn default() -> Self {
        Self {
            raised: Cell::new(false),
            waker: Cell::new(Waker::noop().clone()),
        }
    }
}

impl Alarm {
    fn raise(&self) {
        self.raised.set(true);
        self.waker.replace(Waker::noop().clone()).wake();
    }
}

/// Spawns tasks on the runtime of the calling thread, the shard's own.
struct Spawner(Rc<Alarm>);

impl env::tasks::Driver for Spawner {
    fn spawn(&self, task: Task) {
        drop(tokio::task::spawn_local(Caught::new(
            task,
            Rc::clone(&self.0),
        )));
    }
}

/// A task whose panics, in its poll or its drop, raise the alarm of its shard. Tokio
/// would drop the panic of a spawned task and miss one in its drop.
struct Caught {
    /// `None` only in the drop.
    task: Option<Task>,
    alarm: Rc<Alarm>,
}

impl Caught {
    fn new(task: Task, alarm: Rc<Alarm>) -> Self {
        Self {
            task: Some(task),
            alarm,
        }
    }
}

impl Future for Caught {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Tokio runs the rest of its queue before the main loop sees the alarm.
        if self.alarm.raised.get() {
            return Poll::Pending;
        }
        let task = self
            .task
            .as_mut()
            .expect("a task is taken only in its drop");
        let poll = panic::catch_unwind(AssertUnwindSafe(|| task.as_mut().poll(cx)));
        poll.unwrap_or_else(|_| {
            self.alarm.raise();
            Poll::Ready(())
        })
    }
}

impl Drop for Caught {
    fn drop(&mut self) {
        let task = self.task.take();
        if panic::catch_unwind(AssertUnwindSafe(|| drop(task))).is_err() {
            self.alarm.raise();
        }
    }
}
