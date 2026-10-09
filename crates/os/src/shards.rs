//! Shards on OS threads, each with a Tokio `LocalRuntime`.

use std::cell::Cell;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use env::shards::{Config, Main};
use env::tasks::{Task, Tasks};
use env::thread::{Error, Handle};
use tokio::runtime::{Builder, LocalOptions, LocalRuntime};

use crate::cores::Cores;
use crate::{thread, unwind};

/// Starts each shard on its own thread.
pub(crate) struct Driver(Arc<Cores>);

impl Driver {
    pub(crate) fn new(cores: Cores) -> Self {
        Self(Arc::new(cores))
    }
}

impl env::shards::Driver for Driver {
    fn cores(&self) -> NonZeroUsize {
        self.0.count
    }

    fn pinnable(&self) -> bool {
        !self.0.cpus.is_empty()
    }

    fn start(&self, config: Config, main: Main) -> Result<Handle, Error> {
        let Config { name, core } = config;
        let cores = Arc::clone(&self.0);
        let build = move |name: &str| build(&cores, name, core);
        thread::start(name, build, |runtime| serve(runtime, main))
    }
}

/// Places the calling thread and builds its runtime.
fn build(
    cores: &Cores,
    name: &str,
    core: Option<usize>,
) -> Result<LocalRuntime, Error> {
    cores.place(name, core)?;
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
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
    // A panic in the drop of a payload can escape Tokio's catches of a task that the
    // shard spawned without `tasks`, in `block_on` or in the drop of `runtime`. Each is
    // caught apart, so that a panic in the drop of a task does not abort the unwind of
    // the first.
    let ran = unwind::catch(|| {
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
    });
    let dropped = unwind::catch(|| drop(runtime));
    alarm.raised.get() || ran.is_none() || dropped.is_none()
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
        unwind::catch(|| task.as_mut().poll(cx)).unwrap_or_else(|| {
            self.alarm.raise();
            Poll::Ready(())
        })
    }
}

impl Drop for Caught {
    fn drop(&mut self) {
        let task = self.task.take();
        if unwind::catch(|| drop(task)).is_none() {
            self.alarm.raise();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cores_with_no_cpus_cannot_pin() {
        let cores = Cores {
            count: NonZeroUsize::MIN,
            cpus: Vec::new(),
        };
        assert!(!env::shards::Shards::new(Driver::new(cores)).pinnable());
    }
}
