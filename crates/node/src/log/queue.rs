//! The queue of a node's log and the thread `log` that writes it, so no event waits on
//! the writer.

mod lines;

use std::io::{self, Write};
use std::panic::PanicHookInfo;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use env::thread::Error;
use tracing::Dispatch;
use tracing_subscriber::fmt::MakeWriter;

use self::lines::{Batch, Lines, Run, Taken, Waited, Writer};
use super::Line;

/// The lines that the queue holds. A line that finds it full drops.
const LINES: usize = 1024;

/// The bytes of the lines that the queue holds. A line that does not fit drops.
const BYTES: usize = 1 << 20;

/// A panic hook of the process, as [`std::panic::set_hook`] takes it.
type Hook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync>;

/// Waits for the lines of a node's log to reach its writer. Clones wait on the same
/// log.
#[derive(Clone, Debug)]
pub struct Flush {
    queue: Arc<Queue>,
    /// The node's dispatch, whose events go into `queue`.
    pub(crate) dispatch: Dispatch,
}

impl Flush {
    /// The log `line` of `writer`, whose dispatch holds each event of `level` or above,
    /// with no thread `log` yet. Each line waits in the queue until a thread starts:
    /// at [`Flush::hold`], [`Flush::resume`], or [`Flush::wait`].
    pub(super) fn new(
        writer: Box<dyn Write + Send>,
        line: Line,
        level: tracing::Level,
        threads: &env::threads::Threads,
    ) -> Self {
        let queue = Arc::new(Queue {
            lines: Arc::new(Lines::new(LINES, BYTES, Writer(writer))),
            threads: threads.logged(Dispatch::none()),
            line: line.clone(),
        });
        let dispatch = super::dispatch(line, level, Pipe(Arc::clone(&queue.lines)));
        Self { queue, dispatch }
    }

    /// Starts the thread `log`, which runs until the [`Hold`] drops and the queue is
    /// empty.
    ///
    /// # Errors
    ///
    /// [`Error::Start`] when the thread cannot start. The log then has no hold.
    pub(crate) fn hold(&self) -> Result<Hold, Error> {
        self.queue.lines.open();
        let hold = Hold(Arc::clone(&self.queue));
        self.queue.resume()?;
        Ok(hold)
    }

    /// Starts the thread `log` when none runs, so the lines that came after each
    /// [`Hold`] dropped reach the writer. Returns `false` when the thread could not
    /// start: the lines then wait for the next call. Call it outside an event: an OS
    /// thread cannot start inside one.
    pub(crate) fn resume(&self) -> bool {
        self.queue.resume().is_ok()
    }

    /// Starts the thread `log` as [`Flush::resume`] does, then blocks until the writer
    /// has written each line that was in the queue at the call, and the count line of
    /// each line that had dropped, or until `bound` passes or the writer panics. Once
    /// each shard has ended, also blocks until the thread `log` has ended, and joins
    /// it. Returns `false` when `bound` passed first, the thread could not start, or
    /// the writer panicked. Blocks on real time: for `main`. Under `sim`, use only a
    /// `bound` of zero, which starts the thread `log` for the next run of the sim.
    ///
    /// # Panics
    ///
    /// When the thread `log` panicked after it ended its run, which is a defect.
    #[expect(
        clippy::must_use_candidate,
        reason = "the end of `main` has nothing to do when the bound passes"
    )]
    pub fn wait(&self, bound: Duration) -> bool {
        let sent = self.queue.lines.sent();
        if !self.resume() {
            return false;
        }
        self.queue.lines.wait(sent, bound) == Waited::Written
    }

    /// The panic hook for `main` to set, over `previous`, the hook before it. It
    /// logs ERROR `panicked` with the field `panic`, the text of the panic, after each
    /// line in the queue; this line never drops. Then it calls `previous`. It sets no
    /// dispatch and starts no thread, so a panic inside an event does not abort. While
    /// a thread `log` runs on another thread, it first waits up to `bound` for the log
    /// to write each line. It calls `previous` unless `bound` passes while that thread
    /// still runs: the writer may block the write of `previous` too.
    #[must_use]
    pub fn hook(&self, previous: Hook, bound: Duration) -> Hook {
        let flush = self.clone();
        Box::new(move |info| {
            if flush.panicked(info, bound) {
                previous(info);
            }
        })
    }

    /// Logs the panic line of `info`, then gives whether the hook before may write, as
    /// [`Flush::hook`] states.
    fn panicked(&self, info: &PanicHookInfo<'_>, bound: Duration) -> bool {
        let lines = &self.queue.lines;
        let sent = lines.panic(self.queue.line.panicked(info).into_bytes());
        if !lines.runs_elsewhere() {
            return true;
        }
        match lines.wait(sent, bound) {
            Waited::Written | Waited::Lost => true,
            Waited::Late => !lines.runs_elsewhere(),
        }
    }
}

/// Keeps the thread `log` running while a line can come: each shard holds an `Arc` of
/// it. The drop lets the thread end once the queue is empty. A later line waits for
/// [`Flush::resume`] or [`Flush::wait`].
#[derive(Debug)]
pub(crate) struct Hold(Arc<Queue>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.lines.close();
    }
}

/// What each [`Flush`], each [`Hold`], and the thread `log` share.
#[derive(Debug)]
pub(super) struct Queue {
    /// The lines and the run of the thread `log`. Closed while no [`Hold`] is left,
    /// so the thread `log` ends when no line waits.
    lines: Arc<Lines>,
    threads: env::threads::Threads,
    line: Line,
}

impl Queue {
    /// Starts a thread `log` when none runs, the writer has not panicked, and a
    /// [`Hold`] is left or a line waits. The thread takes the writer from `lines`, so
    /// a start that fails keeps it.
    fn resume(self: &Arc<Self>) -> Result<(), Error> {
        let spawn = |run: Run| {
            let queue = Arc::clone(self);
            let drain = move || {
                let (writer, batch) = run.begin();
                Drain {
                    queue,
                    writer: Some(writer),
                    batch,
                }
            };
            self.threads.start("log", drain)
        };
        self.lines.resume(spawn)
    }
}

/// The writer of the dispatch: it puts each line in the queue.
#[derive(Debug)]
pub(super) struct Pipe(Arc<Lines>);

impl<'a> MakeWriter<'a> for Pipe {
    type Writer = &'a Pipe;

    fn make_writer(&'a self) -> &'a Pipe {
        self
    }
}

impl Write for &Pipe {
    /// `fmt` writes each line in one call.
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        self.0.push(line.to_vec());
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The future of the thread `log`: it writes each line that waits, and ends once no
/// line waits and no [`Hold`] is left.
struct Drain {
    queue: Arc<Queue>,
    writer: Option<Writer>,
    batch: Batch,
}

impl Drain {
    /// Writes the lines of the batch. A write that fails drops its line.
    fn write(&mut self) {
        let Writer(writer) = self
            .writer
            .as_mut()
            .expect("invariant: a drain has its writer");
        let batch = &mut self.batch;
        let sent = batch.sent();
        for line in batch.lines.drain(..) {
            drop(writer.write_all(&line));
        }
        if batch.dropped > 0 {
            let count = self.queue.line.dropped(batch.dropped);
            drop(writer.write_all(count.as_bytes()));
        }
        for line in batch.panics.drain(..) {
            drop(writer.write_all(&line));
        }
        self.queue.lines.wrote(sent);
    }
}

impl Drop for Drain {
    /// After a panic of the writer, ends the run with no writer, so a panic hook does
    /// not wait for a thread that writes no more lines.
    fn drop(&mut self) {
        if self.writer.take().is_some() {
            self.queue.lines.lose();
        }
    }
}

impl Future for Drain {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let drain = self.get_mut();
        loop {
            let batch = &mut drain.batch;
            match drain.queue.lines.take(batch, cx.waker(), &mut drain.writer) {
                Taken::Lines => drain.write(),
                Taken::Empty => return Poll::Pending,
                Taken::Closed => return Poll::Ready(()),
            }
        }
    }
}
