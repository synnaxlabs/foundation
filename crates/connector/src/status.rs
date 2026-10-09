//! The status channels of a connector, `<connector>.status.<name>`, on one index of
//! their own, and their writes.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::fmt;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{self, Poll, Wake, Waker};

use env::clock::Clock;
use types::authority::Authority;
use types::frame::{Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Monotonic, Span, Stamp};

use hub::home::{self, Outcome, Refusal};
use hub::writer::Failure;

use crate::kind;

/// The name of the index of the status channels.
const TIME: &str = "time";

/// The channels that the supervisor writes, with the sample type of each.
const SUPERVISOR: [(&str, Type); 3] = [
    ("state", Type::Scalar(Scalar::U8)),
    ("class", Type::Scalar(Scalar::U8)),
    ("restarts", Type::Scalar(Scalar::U64)),
];

/// The least time between two writes of a change of counts alone.
const PERIOD: Span = Span::SECOND;

/// The status channels of `connector`: the name of their index, then the name and
/// sample type of each channel on it, the supervisor's first, then each of `counts`
/// as `u64`. `counts` is as [`kind::Table::check`] gives them.
///
/// # Errors
///
/// [`types::name::Error::Long`] for the first status name over [`Name::MAX_BYTES`].
pub fn channels(
    connector: &Name,
    counts: &[Name],
) -> Result<(Name, Vec<(Name, Type)>), types::name::Error> {
    let status = |last: &str| format!("{connector}.status.{last}").parse::<Name>();
    let time = status(TIME)?;
    let supervisor = SUPERVISOR.iter().map(|&(name, sample)| (name, sample));
    let counts = counts
        .iter()
        .map(|count| (count.as_str(), Type::Scalar(Scalar::U64)));
    let channels = supervisor
        .chain(counts)
        .map(|(last, sample)| Ok((status(last)?, sample)))
        .collect::<Result<_, _>>()?;
    Ok((time, channels))
}

/// Panics when `kind` names a count of more than one segment, a count that the index
/// or a channel of the supervisor names in any case, or one count twice in any case.
pub(crate) fn check(kind: &str, counts: &[Name]) {
    for (i, count) in counts.iter().enumerate() {
        let same = |name: &str| name.eq_ignore_ascii_case(count.as_str());
        assert!(
            count.segments().nth(1).is_none(),
            "the kind {kind:?} names the count `{count}`, which is not one segment"
        );
        assert!(
            !same(TIME) && !SUPERVISOR.iter().any(|&(name, _)| same(name)),
            "the kind {kind:?} names the count `{count}`, a status channel of the \
             supervisor"
        );
        assert!(
            !counts[..i].iter().any(|earlier| same(earlier.as_str())),
            "the kind {kind:?} names the count `{count}` twice"
        );
    }
}

/// The status channels of one connector, which its supervisor and its kind share.
/// Each write gives one sample on every status channel, with the last value of each.
#[derive(Debug)]
pub struct Status(Rc<Values>);

impl Status {
    /// A status with each of `counts` at 0, that no writer writes yet.
    pub(crate) fn new(counts: Vec<Name>) -> Self {
        Self(Rc::new(Values {
            counts: counts.into_iter().map(|n| (n, Cell::new(0))).collect(),
            staged: Cell::new(false),
            waker: Cell::new(None),
        }))
    }

    /// The same status, for one more holder.
    pub(crate) fn share(&self) -> Self {
        Self(Rc::clone(&self.0))
    }

    /// The count `name`, to set from the kind's data path. The call searches the names
    /// of the counts, so take each count once, before the data path.
    ///
    /// # Panics
    ///
    /// When the kind's `check` did not name `name` as a count: the name is internal.
    #[must_use]
    pub fn count(&self, name: &str) -> Count {
        let at = self
            .0
            .counts
            .iter()
            .position(|(count, _)| count.as_str() == name);
        let at = at.unwrap_or_else(|| {
            panic!("the kind did not name the count `{name}` in its check")
        });
        Count {
            values: Rc::clone(&self.0),
            at,
        }
    }
}

/// One count of a kind, `<connector>.status.<count>`. It is not `Send`: as `Context`,
/// it stays on the shard of its run.
#[derive(Debug)]
pub struct Count {
    values: Rc<Values>,
    at: usize,
}

impl Count {
    /// Sets the count to `value`. The status gives it within one second, and gives at
    /// most one frame of counts each second, so a value set again before that frame
    /// gives only the last. Never blocks.
    pub fn set(&self, value: u64) {
        self.values.counts[self.at].1.set(value);
        self.values.stage();
    }
}

/// What a connector's run is doing, as `state` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running = 0,
    Waiting = 1,
    Stopped = 2,
    Ending = 3,
}

/// How the last run ended, as `class` gives it: `None` also before the first end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    None = 0,
    Config = 1,
    Device = 2,
    Retry = 3,
}

impl Class {
    /// The class of a run that ended with `end`.
    fn of(end: &Result<(), kind::Error>) -> Self {
        match end {
            Ok(()) => Self::None,
            Err(kind::Error::Config(_)) => Self::Config,
            Err(kind::Error::Device(_)) => Self::Device,
            Err(kind::Error::Retry(_)) => Self::Retry,
        }
    }
}

/// The last value of each count, and whether the status waits for a write.
struct Values {
    counts: Box<[(Name, Cell<u64>)]>,
    staged: Cell<bool>,
    /// The flush, while it waits for a staged count.
    waker: Cell<Option<Waker>>,
}

impl Values {
    /// Marks the status to write, and wakes the flush.
    fn stage(&self) {
        if !self.staged.replace(true)
            && let Some(waker) = self.waker.take()
        {
            waker.wake();
        }
    }
}

impl fmt::Debug for Values {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Values")
            .field("counts", &self.counts)
            .field("staged", &self.staged.get())
            .finish_non_exhaustive()
    }
}

/// The writer of one connector's status channels: the supervisor sets each change of
/// state, and [`Writer::flush`] writes the counts.
pub(crate) struct Writer {
    session: RefCell<Session>,
    values: Rc<Values>,
    clock: Clock,
}

impl Writer {
    /// Opens the writer of the status channels of `connector`, whose kind named
    /// `counts`, as the connector, and gives the status that it writes. `clock` times
    /// the writes.
    pub(crate) async fn open(
        hub: &hub::Hub,
        connector: &Name,
        counts: Vec<Name>,
        clock: Clock,
    ) -> Result<(Self, Status), hub::writer::Error> {
        let names = channels(connector, &counts);
        let (time, channels) = names.unwrap_or_else(|error| {
            let refused = "invariant: the plan refused the name of connector";
            panic!("{refused} `{connector}`: {error}")
        });
        let names = std::iter::once(time).chain(channels.into_iter().map(|(n, _)| n));
        let config = hub::writer::Config {
            subject: connector.clone(),
            authority: Authority::ABSOLUTE,
            lease: None,
            channels: names.collect(),
        };
        let session = hub.writer(config).await?;
        let entries: Box<[usize]> = session.entries().into();
        let set = session.set();
        let mut series: Box<[_]> = entries
            .iter()
            .map(|&entry| {
                let width = set.entries()[entry].data_type.width();
                (
                    entry,
                    width.expect("invariant: a status sample has one width"),
                )
            })
            .collect();
        series.sort_unstable();
        let group = set.entries()[entries[0]].group;
        let status = Status::new(counts);
        let session = Session {
            hub: session,
            entries,
            series,
            group,
            last: None,
            wrote: Monotonic::default(),
            closed: false,
            state: State::Running,
            class: Class::None,
            restarts: 0,
            started: false,
        };
        let writer = Self {
            session: RefCell::new(session),
            values: Rc::clone(&status.0),
            clock,
        };
        Ok((writer, status))
    }

    /// Writes `state` 0, with one more restart after the first start.
    pub(crate) fn start(&self) {
        self.set(|session| {
            if session.started {
                session.restarts = session.restarts.strict_add(1);
            }
            session.started = true;
            session.state = State::Running;
        });
    }

    /// Writes `state` 3 with the class of a run that ended with `end`.
    pub(crate) fn end(&self, end: &Result<(), kind::Error>) {
        self.set(|session| {
            session.class = Class::of(end);
            session.state = State::Ending;
        });
    }

    /// Writes `state` 1.
    pub(crate) fn wait(&self) {
        self.set(|session| session.state = State::Waiting);
    }

    /// Writes `state` 2.
    pub(crate) fn stop(&self) {
        self.set(|session| session.state = State::Stopped);
    }

    fn set(&self, change: impl FnOnce(&mut Session)) {
        let mut session = self.session.borrow_mut();
        change(&mut session);
        session.write(&self.values, self.clock.now());
    }

    /// Polls `run` to its end. Meanwhile it writes the status that a kind staged or
    /// the home did not apply, at most once each [`PERIOD`] after the last write.
    pub(crate) async fn during<T>(&self, run: impl Future<Output = T>) -> T {
        beside(run, self.flush()).await
    }

    #[expect(
        clippy::infinite_loop,
        reason = "the flush ends when its call drops it"
    )]
    async fn flush(&self) -> Infallible {
        let values = &self.values;
        loop {
            poll_fn(|cx| {
                if values.staged.get() {
                    return Poll::Ready(());
                }
                values.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            })
            .await;
            self.write_staged().await;
        }
    }

    /// Writes the status until the home applied it or the writer writes no more, at
    /// most once each [`PERIOD`] after the last write.
    pub(crate) async fn settle(&self) {
        while self.values.staged.get() {
            self.write_staged().await;
        }
    }

    /// Waits until [`PERIOD`] after the last write, then writes the status if it is
    /// still staged.
    async fn write_staged(&self) {
        // A write by the supervisor while this sleeps moves the next write later.
        loop {
            let next = self.session.borrow().wrote + PERIOD;
            if self.clock.now() >= next {
                break;
            }
            self.clock.sleep_until(next).await;
        }
        if self.values.staged.get() {
            let now = self.clock.now();
            self.session.borrow_mut().write(&self.values, now);
        }
    }
}

/// The hub's writer session of the status channels, and the supervisor's part of the
/// status.
struct Session {
    hub: hub::writer::Writer,
    /// The entries of the index and the supervisor's channels, then the counts.
    entries: Box<[usize]>,
    /// Each series of a frame, by entry, with its length.
    series: Box<[(usize, usize)]>,
    group: u32,
    last: Option<Stamp>,
    /// When the last write was, by the node's clock.
    wrote: Monotonic,
    /// Set once a status channel is removed or a commit failed: the writer writes no
    /// more.
    closed: bool,
    state: State,
    class: Class,
    restarts: u64,
    /// Set at the first start, after which each start is a restart.
    started: bool,
}

impl Session {
    /// Writes the last value of each status channel. A frame that the home does not
    /// apply leaves the status staged, so the flush writes it again.
    ///
    /// # Panics
    ///
    /// When the home refuses the frame for a cause that only a defect gives.
    fn write(&mut self, values: &Values, now: Monotonic) {
        values.staged.set(false);
        self.wrote = now;
        if self.closed {
            return;
        }
        let mut stamp = self.hub.now();
        if let Some(last) = self.last {
            stamp = stamp.max(Stamp::from_nanos(last.nanos().strict_add(1)));
        }
        self.last = Some(stamp);
        let mut draft = self
            .hub
            .draft(Form::Raw, &self.series)
            .expect("invariant: the series follow the key set");
        let supervisor = [
            u64::from(self.state as u8),
            u64::from(self.class as u8),
            self.restarts,
        ];
        let counts = values.counts.iter().map(|(_, count)| count.get());
        let mut samples = supervisor.into_iter().chain(counts);
        for (i, &entry) in self.entries.iter().enumerate() {
            let bytes = draft
                .series_mut(entry)
                .expect("invariant: each entry has a series");
            if i == 0 {
                bytes.copy_from_slice(&stamp.nanos().to_le_bytes());
                continue;
            }
            let sample = samples.next().expect("invariant: a sample per channel");
            let len = bytes.len();
            bytes.copy_from_slice(&sample.to_le_bytes()[..len]);
        }
        draft.set_count(self.group, 1);
        match self.hub.write(Label::Path(Path::Live), draft) {
            Ok([Outcome::Applied { .. }]) => {}
            Ok(
                [
                    Outcome::Lost { .. }
                    | Outcome::Refused {
                        refusal:
                            Refusal::Waiting | Refusal::Reserved | Refusal::Order(_),
                        ..
                    },
                ],
            ) => values.stage(),
            Ok([Outcome::Refused { refusal, .. }]) => {
                panic!("the home refuses a status frame: {refusal}")
            }
            Ok(outcomes) => {
                panic!("invariant: a frame of one group has one outcome: {outcomes:?}")
            }
            Err(Failure::Removed(_) | Failure::Home(home::Error::Disk(_))) => {
                self.closed = true;
            }
            Err(Failure::Home(error)) => {
                panic!("the home refuses a status frame: {error}")
            }
        }
    }
}

/// Polls `run` to its end, and `side` with it. `side` has a waker of its own, and is
/// polled only after a wake of it, so a poll of `run` alone costs one atomic swap.
async fn beside<T>(
    run: impl Future<Output = T>,
    side: impl Future<Output = Infallible>,
) -> T {
    let woke = Arc::new(Woke {
        woken: AtomicBool::new(true),
        task: Mutex::new(None),
    });
    let waker = Waker::from(Arc::clone(&woke));
    let mut task: Option<Waker> = None;
    let (mut run, mut side) = (pin!(run), pin!(side));
    poll_fn(|cx| {
        if !task.as_ref().is_some_and(|task| task.will_wake(cx.waker())) {
            task = Some(cx.waker().clone());
            let mut stored = woke.task.lock().expect("no panic under the lock");
            stored.clone_from(&task);
        }
        if woke.woken.swap(false, Ordering::AcqRel) {
            let side = side.as_mut().poll(&mut task::Context::from_waker(&waker));
            if let Poll::Ready(never) = side {
                match never {}
            }
        }
        run.as_mut().poll(cx)
    })
    .await
}

/// The waker of the side future of [`beside`]: it marks the side as woken, then
/// wakes the task that polls both.
struct Woke {
    woken: AtomicBool,
    task: Mutex<Option<Waker>>,
}

impl Wake for Woke {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        if let Some(task) = &*self.task.lock().expect("no panic under the lock") {
            task.wake_by_ref();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A side of [`beside`] that counts its polls and keeps its last waker.
    fn side(
        polls: &Rc<Cell<u32>>,
        waker: &Rc<RefCell<Option<Waker>>>,
    ) -> impl Future<Output = Infallible> {
        let (polls, waker) = (Rc::clone(polls), Rc::clone(waker));
        poll_fn(move |cx| {
            polls.set(polls.get() + 1);
            *waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
    }

    /// A run that is pending `n` times, then ready.
    fn pending(mut n: u32) -> impl Future<Output = ()> {
        poll_fn(move |_| {
            if n == 0 {
                return Poll::Ready(());
            }
            n -= 1;
            Poll::Pending
        })
    }

    #[test]
    fn polls_the_side_only_after_a_wake_of_its_own() {
        let (polls, waker) = (Rc::default(), Rc::default());
        let mut both = pin!(beside(pending(3), side(&polls, &waker)));
        let mut cx = task::Context::from_waker(Waker::noop());
        assert!(both.as_mut().poll(&mut cx).is_pending());
        assert!(both.as_mut().poll(&mut cx).is_pending());
        assert_eq!(polls.get(), 1, "a poll of the run alone polls no side");
        waker
            .borrow()
            .as_ref()
            .expect("the side keeps a waker")
            .wake_by_ref();
        assert!(both.as_mut().poll(&mut cx).is_pending());
        assert_eq!(polls.get(), 2, "the side is polled after its wake");
        assert!(both.as_mut().poll(&mut cx).is_ready());
        assert_eq!(polls.get(), 2);
    }

    /// A waker that counts its wakes.
    #[derive(Default)]
    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn wakes_the_task_that_polled_last_at_a_wake_of_the_side() {
        let (polls, waker) = (Rc::default(), Rc::default());
        let (first, last) = (Arc::new(Wakes::default()), Arc::new(Wakes::default()));
        let mut both = pin!(beside(pending(3), side(&polls, &waker)));
        for task in [&first, &last] {
            let task = Waker::from(Arc::clone(task));
            let pending = both.as_mut().poll(&mut task::Context::from_waker(&task));
            assert!(pending.is_pending());
        }
        waker
            .borrow()
            .as_ref()
            .expect("the side keeps a waker")
            .wake_by_ref();
        let woken = |task: &Wakes| task.0.load(Ordering::Relaxed);
        assert_eq!((woken(&first), woken(&last)), (0, 1));
    }

    fn name(text: &str) -> Name {
        text.parse().expect("a valid name")
    }

    #[test]
    fn gives_the_index_then_the_supervisor_channels_then_the_counts() {
        let counts = [name("samples"), name("errors")];
        let (time, channels) = channels(&name("plant.modbus"), &counts).expect("names");
        let (u8, u64) = (Type::Scalar(Scalar::U8), Type::Scalar(Scalar::U64));
        let want = [
            (name("plant.modbus.status.state"), u8),
            (name("plant.modbus.status.class"), u8),
            (name("plant.modbus.status.restarts"), u64),
            (name("plant.modbus.status.samples"), u64),
            (name("plant.modbus.status.errors"), u64),
        ];
        assert_eq!(time, name("plant.modbus.status.time"));
        assert_eq!(channels, want);
    }

    #[test]
    fn debugs_each_value_but_the_waker() {
        let status = Status::new(vec![name("samples")]);
        status.count("samples").set(3);
        let want = "Status(Values { counts: \
                    [(Name(\"samples\"), Cell { value: 3 })], staged: true, .. })";
        assert_eq!(format!("{status:?}"), want);
    }

    #[test]
    fn refuses_the_first_status_name_longer_than_a_name() {
        let long = |bytes| Err(types::name::Error::Long { bytes });
        // The index, `<connector>.status.time`, comes first.
        assert_eq!(channels(&name(&"c".repeat(244)), &[]), long(256));
        assert_eq!(channels(&name(&"c".repeat(243)), &[]), long(256));
        let counts = [name(&"a".repeat(50)), name(&"b".repeat(60))];
        assert_eq!(channels(&name(&"c".repeat(200)), &counts), long(258));
        let error = channels(&name(&"c".repeat(244)), &[]).expect_err("too long");
        assert_eq!(
            error.to_string(),
            "a name or pattern is 256 bytes long, more than the limit of 255 bytes"
        );
    }
}
