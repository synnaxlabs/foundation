//! The lines between a node's events and the thread `log`, the run of that thread,
//! and its wake.

use std::collections::VecDeque;
use std::io::Write;
use std::mem;
use std::sync::Arc;
use std::task::Waker;
use std::thread::{self, ThreadId};
use std::time::Duration;

use env::thread::{Error, Handle};

#[cfg(loom)]
use loom::sync::{Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex, MutexGuard};

/// The lines that wait for the thread `log`, in the order they came, and the run of
/// that thread, with its handle. Each change, each take, and each start and end of a
/// run happen under one lock, so a line or a close always wakes a take that waits, and
/// a start always sees a run that has ended.
#[derive(Debug)]
pub(super) struct Lines {
    inner: Mutex<Inner>,
    /// Signals each change of `Inner::written`, `Inner::running`, and `Inner::resuming`.
    written: Condvar,
    /// The lines that can wait. A line that finds this many drops.
    bound: usize,
    /// The bytes that the lines that wait can hold. A line that does not fit drops.
    bytes: usize,
}

#[derive(Debug)]
struct Inner {
    lines: VecDeque<Vec<u8>>,
    /// The bytes of `lines`.
    bytes: usize,
    /// The panic lines, which never drop.
    panics: Vec<Vec<u8>>,
    /// The lines that dropped since the last take.
    dropped: u64,
    /// The lines that came, dropped ones too.
    sent: u64,
    closed: bool,
    /// The waker of the take that waits.
    waker: Option<Waker>,
    /// The writer while no thread `log` holds it.
    writer: Option<Writer>,
    /// A thread `log` runs or starts. When false, `writer` is there, other than after
    /// the writer panicked: then no thread starts again.
    running: bool,
    /// The thread `log` of the last run that began, or `None` from a start to its
    /// [`Run::begin`]. Read only while `running`.
    thread: Option<ThreadId>,
    /// The lines that the thread `log` has written or counted as dropped.
    written: u64,
    /// The runs that started, so [`Lines::resume`] knows a later one.
    runs: u64,
    /// The calls of [`Lines::resume`] that started a run and have not returned.
    resuming: u64,
    /// The thread `log` that runs or ran last, until a [`Lines::resume`] or a
    /// [`Lines::wait`] joins it.
    handle: Option<Handle>,
}

/// A run of the thread `log` that [`Lines::resume`] gives to its spawn. Its new thread
/// calls [`Run::begin`]. A drop before that ends the run with the writer kept, as for
/// a thread that did not start.
#[must_use = "a dropped Run ends its run"]
#[derive(Debug)]
pub(super) struct Run {
    /// `None` once the run has begun.
    lines: Option<Arc<Lines>>,
    key: u64,
}

impl Run {
    /// Gives the writer and an empty batch to the calling thread, the thread `log` of
    /// the run.
    pub(super) fn begin(mut self) -> (Writer, Batch) {
        let lines = self.lines.take().expect("invariant: a run begins once");
        let thread = thread::current().id();
        let mut inner = lines.lock();
        inner.thread = Some(thread);
        let writer = inner.writer.take();
        drop(inner);
        let writer =
            writer.expect("invariant: the writer waits for the run that started");
        (writer, lines.batch())
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(lines) = self.lines.take() {
            lines.lock().running = false;
            lines.written.notify_all();
        }
    }
}

/// How a [`Lines::wait`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Waited {
    /// The lines were written and, while the lines are closed, the run has ended and
    /// each resume has returned.
    Written,
    /// The bound passed first.
    Late,
    /// The writer panicked before it wrote the lines.
    Lost,
}

/// The writer of a node's log.
pub(super) struct Writer(pub(super) Box<dyn Write + Send>);

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Writer")
    }
}

/// The lines of one take, in the order to write them: `lines`, then the count line
/// of `dropped` when it is not 0, then `panics`. Each line of `lines` came before
/// each line that dropped.
#[derive(Debug, Default)]
pub(super) struct Batch {
    pub(super) lines: VecDeque<Vec<u8>>,
    pub(super) dropped: u64,
    pub(super) panics: Vec<Vec<u8>>,
}

impl Batch {
    /// The lines of the take that came into `sent`, dropped ones too.
    pub(super) fn sent(&self) -> u64 {
        (self.lines.len() + self.panics.len()) as u64 + self.dropped
    }
}

/// What a take gives.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Taken {
    /// The batch holds lines or a count of lines that dropped.
    Lines,
    /// No line waits and none dropped. The next line, drop, or close wakes the waker
    /// of the take.
    Empty,
    /// No line waits, none dropped, and [`Lines::close`] came after the last
    /// [`Lines::open`]. The run of the thread `log` has ended.
    Closed,
}

impl Lines {
    /// Lines with no line, which hold up to `bound` lines and `bytes` bytes of lines,
    /// closed, and with `writer` for the first thread `log`.
    pub(super) fn new(bound: usize, bytes: usize, writer: Writer) -> Self {
        let inner = Inner {
            lines: VecDeque::with_capacity(bound),
            bytes: 0,
            panics: Vec::new(),
            dropped: 0,
            sent: 0,
            closed: true,
            waker: None,
            writer: Some(writer),
            running: false,
            thread: None,
            written: 0,
            runs: 0,
            resuming: 0,
            handle: None,
        };
        Self {
            inner: Mutex::new(inner),
            written: Condvar::new(),
            bound,
            bytes,
        }
    }

    /// An empty batch for [`Lines::take`], with room for `bound` lines, so neither
    /// queue of lines grows after it.
    pub(super) fn batch(&self) -> Batch {
        Batch {
            lines: VecDeque::with_capacity(self.bound),
            ..Batch::default()
        }
    }

    /// Puts `line` after the lines that wait, or counts it as dropped when `bound`
    /// lines wait or it does not fit in `bytes`. After a drop, each line drops until
    /// the next take, so the count line of the take marks one gap.
    pub(super) fn push(&self, line: Vec<u8>) {
        let mut inner = self.lock();
        let full =
            inner.lines.len() == self.bound || line.len() > self.bytes - inner.bytes;
        inner.sent += 1;
        if full || inner.dropped > 0 {
            inner.dropped += 1;
            wake(inner);
            return;
        }
        inner.bytes += line.len();
        inner.lines.push_back(line);
        wake(inner);
    }

    /// Puts the panic line `line` after each line that waits; it never drops. Gives
    /// the lines sent with it.
    pub(super) fn panic(&self, line: Vec<u8>) -> u64 {
        let mut inner = self.lock();
        inner.panics.push(line);
        inner.sent += 1;
        let sent = inner.sent;
        wake(inner);
        sent
    }

    /// Lets a take wait for a line after the queue is empty.
    pub(super) fn open(&self) {
        self.lock().closed = false;
    }

    /// Lets a take end once no line waits.
    pub(super) fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        wake(inner);
    }

    /// The lines that came, dropped ones too.
    pub(super) fn sent(&self) -> u64 {
        self.lock().sent
    }

    /// Starts a run of the thread `log` with `spawn` when none runs, the writer has not
    /// panicked, and the lines are open or a line waits, and keeps the handle that
    /// `spawn` gives. Outside the lock, joins the handle of a run that has ended: the
    /// one it replaces, or that of `spawn` when a later run has started.
    ///
    /// # Errors
    ///
    /// The error of `spawn`, which drops its [`Run`], so the next resume can start one.
    ///
    /// # Panics
    ///
    /// When the thread `log` that it joins panicked after it ended its run, which is a
    /// defect.
    pub(super) fn resume(
        self: &Arc<Self>,
        spawn: impl FnOnce(Run) -> Result<Handle, Error>,
    ) -> Result<(), Error> {
        let Some(run) = self.start() else {
            return Ok(());
        };
        let key = run.key;
        let resume = Resume(self);
        let handle = spawn(run)?;
        let mut inner = self.lock();
        let ended = if inner.runs == key {
            inner.handle.replace(handle)
        } else {
            Some(handle)
        };
        drop(inner);
        if let Some(ended) = ended {
            join(ended);
        }
        drop(resume);
        Ok(())
    }

    /// Starts a run when [`Lines::resume`] may.
    fn start(self: &Arc<Self>) -> Option<Run> {
        let mut inner = self.lock();
        let waiting = inner.sent > inner.written;
        if inner.running || inner.writer.is_none() || (inner.closed && !waiting) {
            return None;
        }
        inner.running = true;
        inner.thread = None;
        inner.runs += 1;
        inner.resuming += 1;
        Some(Run {
            lines: Some(Arc::clone(self)),
            key: inner.runs,
        })
    }

    /// Moves each line that waits and the count of the lines that dropped into
    /// `batch`, whose lines must be empty. When neither is there and the lines are not
    /// closed, keeps `waker` to wake at the next line, drop, or close. When the lines
    /// are closed, ends the run in the same step, which gives back `writer`.
    pub(super) fn take(
        &self,
        batch: &mut Batch,
        waker: &Waker,
        writer: &mut Option<Writer>,
    ) -> Taken {
        let mut inner = self.lock();
        if inner.lines.is_empty() && inner.panics.is_empty() && inner.dropped == 0 {
            if inner.closed {
                inner.writer = writer.take();
                inner.running = false;
                drop(inner);
                self.written.notify_all();
                return Taken::Closed;
            }
            inner.waker = Some(waker.clone());
            return Taken::Empty;
        }
        mem::swap(&mut inner.lines, &mut batch.lines);
        inner.bytes = 0;
        mem::swap(&mut inner.panics, &mut batch.panics);
        batch.dropped = mem::take(&mut inner.dropped);
        Taken::Lines
    }

    /// Counts `sent` more lines as written or dropped, and wakes each wait.
    pub(super) fn wrote(&self, sent: u64) {
        self.lock().written += sent;
        self.written.notify_all();
    }

    /// Ends the run after the writer panicked: the writer is lost, so no thread starts
    /// again and no wait waits for one.
    pub(super) fn lose(&self) {
        self.lock().running = false;
        self.written.notify_all();
    }

    /// Whether a thread `log` other than the calling thread runs or starts.
    pub(super) fn runs_elsewhere(&self) -> bool {
        let thread = thread::current().id();
        let inner = self.lock();
        inner.running && inner.thread != Some(thread)
    }

    /// Blocks until the first `sent` lines are written and, while the lines are
    /// closed, the run has ended and each resume has returned, or until `bound` passes
    /// or the writer panicked. Blocks on real time. Then, outside the lock, joins the
    /// thread `log` when no run runs, other than after the writer panicked: that panic
    /// has reached the panic hook.
    ///
    /// # Panics
    ///
    /// When the thread `log` panicked after it ended its run, which is a defect.
    pub(super) fn wait(&self, sent: u64, bound: Duration) -> Waited {
        let busy = |inner: &mut Inner| {
            let live = inner.running || inner.writer.is_some();
            let ending = inner.running || inner.resuming > 0;
            live && (inner.written < sent || (inner.closed && ending))
        };
        let (mut inner, late) = wait_while(&self.written, self.lock(), bound, busy);
        let waited = if late {
            Waited::Late
        } else if inner.written >= sent {
            Waited::Written
        } else {
            Waited::Lost
        };
        let ended = if inner.running || inner.writer.is_none() {
            None
        } else {
            inner.handle.take()
        };
        drop(inner);
        if let Some(ended) = ended {
            join(ended);
        }
        waited
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("invariant: nothing panics while it holds the lines lock")
    }
}

/// A [`Lines::resume`] that started a run. Its drop, after the join of the resume
/// or as the spawn unwinds, ends the resume, so no wait waits for it.
struct Resume<'a>(&'a Lines);

impl Drop for Resume<'_> {
    fn drop(&mut self) {
        self.0.lock().resuming -= 1;
        self.0.written.notify_all();
    }
}

/// Joins `handle`, a thread `log` that has ended its run.
///
/// # Panics
///
/// When it panicked after it ended its run, which is a defect.
fn join(handle: Handle) {
    handle
        .join()
        .expect("invariant: the thread log ends with no panic");
}

/// Waits on `written` while `busy`, up to `bound`. Gives whether `bound` passed.
/// Under loom, which has no time, it waits with no bound.
fn wait_while<'a>(
    written: &Condvar,
    inner: MutexGuard<'a, Inner>,
    bound: Duration,
    busy: impl FnMut(&mut Inner) -> bool,
) -> (MutexGuard<'a, Inner>, bool) {
    #[cfg(not(loom))]
    #[expect(
        clippy::disallowed_methods,
        reason = "a stop and a panic hook cannot await"
    )]
    let (inner, late) = {
        let (inner, waited) = written
            .wait_timeout_while(inner, bound, busy)
            .expect("invariant: nothing panics while it holds the lines lock");
        (inner, waited.timed_out())
    };
    #[cfg(loom)]
    let (inner, late) = {
        let _ = bound;
        let (mut inner, mut busy) = (inner, busy);
        while busy(&mut inner) {
            inner = written
                .wait(inner)
                .expect("invariant: nothing panics while it holds the lines lock");
        }
        (inner, false)
    };
    (inner, late)
}

/// Wakes the take that waits, after the lock of `inner` drops.
fn wake(mut inner: MutexGuard<'_, Inner>) {
    let waker = inner.waker.take();
    drop(inner);
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// A writer that drops each line.
#[cfg(test)]
fn sink() -> Writer {
    Writer(Box::new(std::io::sink()))
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::task::Waker;
    use std::time::Duration;

    use env::thread::{Error, Handle};

    use super::{Batch, Lines, Taken, Waited, sink};

    #[test]
    fn a_panic_line_counts_among_the_lines_sent() {
        let lines = Lines::new(1, 1, sink());
        lines.push(b"a".to_vec());
        lines.push(b"b".to_vec());
        assert_eq!((lines.panic(b"p".to_vec()), lines.sent()), (3, 3));
        let mut batch = Batch::default();
        assert_eq!(
            lines.take(&mut batch, Waker::noop(), &mut None),
            Taken::Lines
        );
        assert_eq!((batch.sent(), batch.dropped), (3, 1));
    }

    #[test]
    fn a_line_too_large_for_the_bytes_drops_with_each_line_until_the_take() {
        let lines = Lines::new(4, 2, sink());
        lines.push(b"aaa".to_vec());
        lines.push(b"b".to_vec());
        let mut batch = Batch::default();
        assert_eq!(
            lines.take(&mut batch, Waker::noop(), &mut None),
            Taken::Lines
        );
        assert_eq!((batch.sent(), batch.dropped), (2, 2));
        for line in [b"cc", b"dd"] {
            lines.push(line.to_vec());
            assert_eq!(
                lines.take(&mut batch, Waker::noop(), &mut None),
                Taken::Lines
            );
            assert_eq!(batch.lines.drain(..).collect::<Vec<_>>(), [line.to_vec()]);
            assert_eq!(batch.dropped, 0);
        }
    }

    #[test]
    fn a_batch_has_room_for_the_bound() {
        assert!(Lines::new(4, 4, sink()).batch().lines.capacity() >= 4);
    }

    /// A handle that adds `run` to `joined` at its join.
    fn handle(joined: &Arc<Mutex<Vec<u64>>>, run: u64) -> Handle {
        let joined = Arc::clone(joined);
        Handle::new(move || {
            joined.lock().unwrap().push(run);
            Ok(())
        })
    }

    #[test]
    fn a_run_whose_thread_did_not_start_leaves_the_lines_for_the_next_start() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let error = Error::Start {
            name: "log".into(),
            reason: "no threads".into(),
        };
        let failed = lines.resume(|run| {
            assert_eq!(lines.resume(|_| panic!("a run runs")), Ok(()));
            drop(run);
            Err(error.clone())
        });
        assert_eq!(failed, Err(error));
        lines.close();
        assert_eq!(lines.wait(lines.sent(), Duration::ZERO), Waited::Written);
        lines.open();
        let mut begun = None;
        let resumed = lines.resume(|run| {
            begun = Some(run.begin());
            Ok(Handle::new(|| Ok(())))
        });
        assert_eq!(resumed, Ok(()));
        assert!(begun.expect("the run began").1.lines.capacity() >= 4);
    }

    #[test]
    fn a_handle_of_a_run_after_a_later_start_comes_back_to_join() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        let joined = Arc::new(Mutex::new(Vec::new()));
        lines.open();
        let first = lines.resume(|run| {
            drop(run);
            let later = lines.resume(|run| {
                drop(run);
                Ok(handle(&joined, 2))
            });
            assert_eq!(later, Ok(()));
            Ok(handle(&joined, 1))
        });
        assert_eq!(first, Ok(()));
        assert_eq!(*joined.lock().unwrap(), [1], "the first run ended");
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
        assert_eq!(*joined.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn a_start_after_a_run_on_this_thread_runs_elsewhere_until_its_begin() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let mut begun = None;
        let resumed = lines.resume(|run| {
            begun = Some(run.begin());
            Ok(Handle::new(|| Ok(())))
        });
        assert_eq!(resumed, Ok(()));
        let (writer, mut batch) = begun.expect("the run began");
        lines.close();
        let taken = lines.take(&mut batch, Waker::noop(), &mut Some(writer));
        assert_eq!(taken, Taken::Closed);
        lines.open();
        let resumed = lines.resume(|run| {
            assert!(lines.runs_elsewhere(), "the new run has not begun");
            drop(run);
            Ok(Handle::new(|| Ok(())))
        });
        assert_eq!(resumed, Ok(()));
    }

    #[test]
    fn a_run_whose_writer_panicked_keeps_its_handle() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        let joined = Arc::new(Mutex::new(Vec::new()));
        lines.open();
        let mut begun = None;
        let resumed = lines.resume(|run| {
            begun = Some(run.begin());
            Ok(handle(&joined, 1))
        });
        assert_eq!(resumed, Ok(()));
        lines.lose();
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
        assert!(joined.lock().unwrap().is_empty(), "the writer panicked");
    }

    #[test]
    fn a_run_that_runs_keeps_its_handle() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let joined = Arc::new(Mutex::new(Vec::new()));
        let mut held = None;
        let resumed = lines.resume(|run| {
            held = Some(run);
            Ok(handle(&joined, 1))
        });
        assert_eq!(resumed, Ok(()));
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
        assert!(joined.lock().unwrap().is_empty(), "the run runs");
        drop(held);
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
        assert_eq!(*joined.lock().unwrap(), [1], "the run ended");
    }

    #[test]
    fn a_closed_wait_waits_for_a_resume_that_a_later_run_follows() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let first = lines.resume(|run| {
            drop(run);
            let later = lines.resume(|run| {
                drop(run);
                Ok(Handle::new(|| Ok(())))
            });
            assert_eq!(later, Ok(()));
            lines.close();
            assert_eq!(lines.wait(0, Duration::ZERO), Waited::Late, "a resume runs");
            Ok(Handle::new(|| Ok(())))
        });
        assert_eq!(first, Ok(()));
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
    }

    #[test]
    fn a_closed_wait_waits_for_a_resume_that_joins_the_run_before_it() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let waited = Arc::new(Mutex::new(None));
        let first = lines.resume(|run| {
            drop(run);
            let (lines, waited) = (Arc::clone(&lines), Arc::clone(&waited));
            Ok(Handle::new(move || {
                lines.close();
                *waited.lock().unwrap() = Some(lines.wait(0, Duration::ZERO));
                Ok(())
            }))
        });
        assert_eq!(first, Ok(()));
        let second = lines.resume(|run| {
            drop(run);
            Ok(Handle::new(|| Ok(())))
        });
        assert_eq!(second, Ok(()));
        assert_eq!(
            *waited.lock().unwrap(),
            Some(Waited::Late),
            "the join is in a resume"
        );
        assert_eq!(lines.wait(0, Duration::ZERO), Waited::Written);
    }

    #[test]
    fn a_wait_after_a_spawn_that_unwinds_has_no_resume_to_wait_for() {
        let lines = Arc::new(Lines::new(4, 4, sink()));
        lines.open();
        let unwound = std::panic::catch_unwind(|| {
            lines.resume(|run| {
                drop(run);
                panic!("the spawn panicked")
            })
        });
        assert!(unwound.is_err(), "the spawn panicked");
        lines.close();
        assert_eq!(lines.wait(0, Duration::from_secs(10)), Waited::Written);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use std::future::poll_fn;
    use std::task::Poll;
    use std::time::Duration;

    use std::sync::Arc;

    use env::thread::Handle;
    use loom::future::block_on;
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::thread;

    use super::{Batch, Lines, Taken, Waited, Writer, sink};

    /// Takes with `writer` until the lines close, as the thread `log` does, and gives
    /// each line and the count of each line that dropped.
    fn drain(lines: &Lines, writer: &mut Option<Writer>) -> (Vec<Vec<u8>>, u64) {
        let mut batch = Batch::default();
        let (mut written, mut dropped) = (Vec::new(), 0);
        block_on(poll_fn(|cx| {
            loop {
                match lines.take(&mut batch, cx.waker(), writer) {
                    Taken::Lines => {
                        let sent = batch.sent();
                        written.extend(batch.lines.drain(..));
                        dropped += batch.dropped;
                        written.append(&mut batch.panics);
                        lines.wrote(sent);
                    }
                    Taken::Empty => return Poll::Pending,
                    Taken::Closed => return Poll::Ready(()),
                }
            }
        }));
        (written, dropped)
    }

    /// Lines from two threads and a close after them each reach a take that waits on
    /// a third.
    #[test]
    fn each_line_before_the_close_reaches_a_take_that_waits() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            lines.open();
            let push = |line: &'static [u8]| {
                let lines = Arc::clone(&lines);
                thread::spawn(move || lines.push(line.to_vec()))
            };
            let pushes = [push(b"a"), push(b"b")];
            let taker = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || drain(&lines, &mut None))
            };
            for push in pushes {
                push.join().unwrap();
            }
            lines.close();
            let (mut written, dropped) = taker.join().unwrap();
            written.sort();
            assert_eq!((written, dropped), (vec![b"a".to_vec(), b"b".to_vec()], 0));
        });
    }

    /// A line that finds the bound drops and counts, and a panic line that finds it
    /// comes after each line before it.
    #[test]
    fn a_full_queue_drops_a_line_but_never_a_panic_line() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(1, 1, sink()));
            lines.open();
            let taker = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || drain(&lines, &mut None))
            };
            lines.push(b"a".to_vec());
            lines.push(b"b".to_vec());
            assert_eq!(lines.panic(b"p".to_vec()), lines.sent());
            lines.close();
            let (written, dropped) = taker.join().unwrap();
            assert_eq!(written.len() as u64 + dropped, 3);
            assert_eq!(written.first(), Some(&b"a".to_vec()));
            assert_eq!(written.last(), Some(&b"p".to_vec()));
        });
    }

    /// A line that comes as the run of the thread `log` ends gets a run that writes
    /// it: the run that ends takes it, or the start after the line finds that run
    /// ended. It calls the private `Lines::start`, since `Lines::resume` gives no
    /// writer back to the caller.
    #[test]
    fn a_line_as_the_run_ends_gets_a_run_that_writes_it() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            lines.open();
            let first = lines.start().expect("the lines are open");
            let run = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || drain(&lines, &mut Some(first.begin().0)).0)
            };
            let late = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || {
                    lines.push(b"a".to_vec());
                    lines.start().map_or_else(Vec::new, |run| {
                        drain(&lines, &mut Some(run.begin().0)).0
                    })
                })
            };
            lines.close();
            let mut written = run.join().unwrap();
            written.extend(late.join().unwrap());
            assert_eq!(written, [b"a".to_vec()]);
            lines.push(b"b".to_vec());
            assert!(
                lines.start().is_some(),
                "the run ended with the writer back"
            );
        });
    }

    /// Resumes, as `Queue::resume` does, with the thread of the run on a loom thread
    /// whose join counts in `joined`.
    fn resume(lines: &Arc<Lines>, joined: &Arc<AtomicU64>) {
        let resumed = lines.resume(|run| {
            let (lines, joined) = (Arc::clone(lines), Arc::clone(joined));
            let thread = thread::spawn(move || drain(&lines, &mut Some(run.begin().0)));
            Ok(Handle::new(move || {
                thread.join().unwrap();
                joined.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        });
        resumed.unwrap();
    }

    /// A wait beside the panic of the writer ends with the lines lost.
    #[test]
    fn a_wait_beside_a_lost_writer_ends_lost() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            lines.push(b"a".to_vec());
            let mut begun = None;
            let resumed = lines.resume(|run| {
                begun = Some(run.begin());
                Ok(Handle::new(|| Ok(())))
            });
            resumed.unwrap();
            let waiter = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || lines.wait(1, Duration::ZERO))
            };
            lines.lose();
            assert_eq!(waiter.join().unwrap(), Waited::Lost);
            drop(begun);
        });
    }

    /// A wait beside a run that drops before its begin ends, as for a thread that did
    /// not start.
    #[test]
    fn a_wait_beside_a_run_that_drops_before_its_begin_ends() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            lines.open();
            let mut held = None;
            let resumed = lines.resume(|run| {
                held = Some(run);
                Ok(Handle::new(|| Ok(())))
            });
            resumed.unwrap();
            lines.close();
            let waiter = {
                let lines = Arc::clone(&lines);
                thread::spawn(move || lines.wait(0, Duration::ZERO))
            };
            drop(held);
            assert_eq!(waiter.join().unwrap(), Waited::Written);
        });
    }

    /// A resume that keeps its handle after a later run has started joins its own
    /// run, which has ended, and not the later one, which runs until the close.
    #[test]
    fn a_start_joins_only_a_run_that_has_ended() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            let joined = Arc::new(AtomicU64::new(0));
            lines.push(b"a".to_vec());
            let later = {
                let (lines, joined) = (Arc::clone(&lines), Arc::clone(&joined));
                thread::spawn(move || {
                    lines.open();
                    resume(&lines, &joined);
                })
            };
            resume(&lines, &joined);
            later.join().unwrap();
            lines.close();
            assert_eq!(lines.wait(lines.sent(), Duration::ZERO), Waited::Written);
            let runs = lines.lock().runs;
            assert_eq!(joined.load(Ordering::SeqCst), runs, "each run ended");
        });
    }

    /// A wait beside the resume of another thread ends only once that resume keeps
    /// the handle of its run, so the wait joins it.
    #[test]
    fn a_wait_beside_a_resume_joins_its_run() {
        loom::model(|| {
            let lines = Arc::new(Lines::new(4, 4, sink()));
            let joined = Arc::new(AtomicU64::new(0));
            lines.push(b"a".to_vec());
            let other = {
                let (lines, joined) = (Arc::clone(&lines), Arc::clone(&joined));
                thread::spawn(move || resume(&lines, &joined))
            };
            assert_eq!(lines.wait(1, Duration::ZERO), Waited::Written);
            assert_eq!(joined.load(Ordering::SeqCst), 1, "the run ended");
            other.join().unwrap();
        });
    }
}
