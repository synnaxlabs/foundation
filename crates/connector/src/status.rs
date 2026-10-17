//! The status channels of a connector, `<connector>.status.<name>`, on one index of
//! their own, and their writes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::future::{pending, poll_fn};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(not(loom))]
use std::sync::Mutex;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{self, Poll, Wake, Waker};

use codec::Variable;
use env::clock::Clock;
use spec::channel::{Channel, Kind};
use spec::definition::Definition;
use types::authority::Authority;
use types::frame::{self, Draft, Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Monotonic, Span, Stamp};

use hub::home::{self, Outcome, Refusal, order};
use hub::writer::Failure;
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};

use crate::stamp::Stamps;
use crate::{cancel, deadline, kind};

mod reading;

pub use reading::Reading;

/// The name of the index of the status channels.
const TIME: &str = "time";

/// The channels that the supervisor writes, with the sample type of each. `error`
/// and `note` are last, so they are entries [`ERROR`] and [`NOTE`] of a frame.
const SUPERVISOR: [(&str, Type); 6] = [
    (segment::STATE, Type::Scalar(Scalar::U8)),
    (segment::CLASS, Type::Scalar(Scalar::U8)),
    (segment::RESTARTS, Type::Scalar(Scalar::U64)),
    (segment::BACKOFF, Type::Scalar(Scalar::Span)),
    (segment::ERROR, Type::String),
    (segment::NOTE, Type::String),
];

/// The last segment of the name of each channel that the supervisor writes.
mod segment {
    pub(super) const STATE: &str = "state";
    pub(super) const CLASS: &str = "class";
    pub(super) const RESTARTS: &str = "restarts";
    pub(super) const BACKOFF: &str = "backoff";
    pub(super) const ERROR: &str = "error";
    pub(super) const NOTE: &str = "note";
}

/// The entry of `error` in a frame, after the index and the other channels of the
/// supervisor but `note`.
const ERROR: usize = SUPERVISOR.len() - 1;
/// The entry of `note` in a frame, after `error`.
const NOTE: usize = SUPERVISOR.len();

/// The most bytes of the text of `error` and of `note`.
const TEXT_MAX: usize = 1024;
/// The mark in place of the middle of a text that is too long.
const MARK: &str = "…";
/// The least time between two writes of a change of counts alone.
const PERIOD: Span = Span::SECOND;
/// The most time that the changes of state wait for the home to apply the status,
/// from the first change that it did not apply, and that the last frame of a call
/// waits. After it, each change goes on, and the next frame that the home applies
/// holds the whole status.
const SETTLE: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The status channels of `connector`: the name of their index, then the name and
/// sample type of each channel on it, the supervisor's first, then each of `counts`
/// as `u64`. `counts` is as [`kind::Table::check`] gives them. Each frame that the
/// supervisor writes holds one sample of each of these channels.
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

/// The status channels that `definitions` defines for the connector `name`: its
/// status index, then each data channel on it, in name order.
///
/// # Panics
///
/// When `definitions` has no status index for `name`, which `config` refuses.
#[must_use]
pub fn defined<'a>(
    definitions: &'a BTreeMap<Name, Definition>,
    name: &Name,
) -> Vec<(&'a Name, &'a Channel)> {
    let index = channels(name, &[]).map(|(index, _)| index);
    let index = index.expect("invariant: `config` refuses a status name that is long");
    let Some((index, Definition::Channel(found))) = definitions.get_key_value(&index)
    else {
        panic!("invariant: `config` defines the status index `{index}`");
    };
    let on = definitions
        .iter()
        .filter_map(|(channel, definition)| match definition {
            Definition::Channel(
                on @ Channel {
                    kind: Kind::Data(data),
                    ..
                },
            ) if *data.index() == found.key => Some((channel, on)),
            _ => None,
        });
    std::iter::once((index, found)).chain(on).collect()
}

/// Panics when `kind` names a count of more than one segment, a count that the index
/// or a channel of the supervisor names in any case, or one count twice in any case.
pub(crate) fn check(kind: &str, counts: &[Name]) {
    let mut seen = BTreeSet::new();
    for count in counts {
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
            seen.insert(count.as_str().to_ascii_lowercase()),
            "the kind {kind:?} names the count `{count}` twice"
        );
    }
}

/// The status channels of one connector, which its supervisor and its kind share.
/// Each write gives one sample on every status channel, with the last value of each.
#[derive(Debug)]
pub(crate) struct Status(Rc<Values>);

impl Status {
    /// A status with each of `counts` at 0, that no writer writes yet.
    pub(crate) fn new(counts: Vec<Name>) -> Self {
        Self(Rc::new(Values {
            counts: counts.into_iter().map(|n| (n, Cell::new(0))).collect(),
            run: RefCell::default(),
            staged: Cell::new(false),
            waker: Cell::new(None),
        }))
    }

    /// The same status, for one more holder.
    pub(crate) fn share(&self) -> Self {
        Self(Rc::clone(&self.0))
    }

    /// The count `name`.
    ///
    /// # Panics
    ///
    /// When the kind's `check` did not name `name` as a count: the name is internal.
    pub(crate) fn count(&self, name: &str) -> Count {
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

    /// Shows `error` as the class `Retry` and its text, from the next write until the
    /// end of the run.
    pub(crate) fn show(&self, error: &dyn fmt::Display) {
        self.set(Shown::Error(cut(&error.to_string())));
    }

    /// Clears the class and the text, from the next write until the end of the run.
    pub(crate) fn clear(&self) {
        self.set(Shown::Cleared);
    }

    /// Shows `note`, cut by [`cut`], from the next write until the end of the run,
    /// unless the run shows it already or no run runs.
    pub(crate) fn note(&self, note: &dyn fmt::Display) {
        let note = cut(&note.to_string());
        self.change(|run| {
            if run.note == note {
                return false;
            }
            run.note = note;
            true
        });
    }

    /// Stages `new`, unless it is the error that the run shows, or no run runs.
    fn set(&self, new: Shown) {
        self.change(|run| {
            if run.error == new {
                return false;
            }
            run.error = new;
            true
        });
    }

    /// Applies `change` to the run, and stages the status when it gives `true`. With
    /// no run, it does nothing. No borrow is held while it stages, which wakes.
    fn change(&self, change: impl FnOnce(&mut Run) -> bool) {
        let changed = self.0.run.borrow_mut().as_mut().is_some_and(change);
        if changed {
            self.0.stage();
        }
    }
}

/// One count of a kind, `<connector>.status.<count>`. It holds across the runs of one
/// supervisor call. It is not `Send`: as `Context`, it stays on the shard of its run.
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

    /// Adds `value` to the count. Saturates at `u64::MAX`. The status gives it as
    /// [`Count::set`] does. Never blocks.
    pub fn add(&self, value: u64) {
        let count = &self.values.counts[self.at].1;
        self.set(count.get().saturating_add(value));
    }
}

/// What a connector's run does, as its `state` channel gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A run is under way.
    Running,
    /// The supervisor waits for the next try of a restart.
    Restarting,
    /// The supervisor runs the connector no more.
    Stopped,
    /// The last run returned, and the supervisor waits for its tasks to end.
    Ending,
}

impl State {
    /// The value of `state`.
    fn code(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Restarting => 1,
            Self::Stopped => 2,
            Self::Ending => 3,
        }
    }

    /// The state of the value `code` of `state`, or `None` when no state has it.
    fn of(code: u64) -> Option<Self> {
        [Self::Running, Self::Restarting, Self::Stopped, Self::Ending]
            .into_iter()
            .find(|state| u64::from(state.code()) == code)
    }
}

/// What the writer of a status does: its state, with the time of the next try by the
/// node's clock while it restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    Waiting(Monotonic),
    Stopped,
    Ending,
}

impl Phase {
    fn state(self) -> State {
        match self {
            Self::Running => State::Running,
            Self::Waiting(_) => State::Restarting,
            Self::Stopped => State::Stopped,
            Self::Ending => State::Ending,
        }
    }
}

/// How the last run of a connector ended with an error, or the error that its running
/// kind shows, as its `class` channel gives it. The value of `class` is 0 when
/// [`Reading::error`] is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `kind::Error::Config`.
    Config = 1,
    /// `kind::Error::Device`.
    Device = 2,
    /// `kind::Error::Retry`, or an error that the running kind shows.
    Retry = 3,
}

impl Class {
    /// The class of a run that ended with `end`, and the text of `error`: `None` and
    /// an empty text for `Ok`, else the text of the error with no class, cut to
    /// [`TEXT_MAX`] bytes.
    fn with_text(end: &Result<(), kind::Error>) -> (Option<Self>, String) {
        let (class, text) = match end {
            Ok(()) => (None, String::new()),
            Err(error @ kind::Error::Config(_)) => {
                (Some(Self::Config), error.to_string())
            }
            Err(kind::Error::Device(source)) => {
                (Some(Self::Device), source.to_string())
            }
            Err(kind::Error::Retry(source)) => (Some(Self::Retry), source.to_string()),
        };
        (class, cut(&text))
    }

    /// The name of the class in a line of the log.
    fn name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Device => "device",
            Self::Retry => "retry",
        }
    }

    /// The value of `class` for `class`.
    fn code(class: Option<Self>) -> u8 {
        class.map_or(0, |class| class as u8)
    }

    /// The class of the value `code` of `class`, or `None` when no class has it.
    fn of(code: u64) -> Option<Self> {
        [Self::Config, Self::Device, Self::Retry]
            .into_iter()
            .find(|class| u64::from(*class as u8) == code)
    }
}

/// The last value of each count, what the run shows, and whether the status waits
/// for a write.
struct Values {
    counts: Box<[(Name, Cell<u64>)]>,
    /// `None` while no run runs: what a task of the last run shows is not written.
    run: RefCell<Option<Run>>,
    staged: Cell<bool>,
    /// The flush, while it waits for a staged count of an open session, so that a set
    /// after the close wakes nothing.
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

/// What one run of the kind shows: [`Writer::start`] sets an empty one, and
/// [`Writer::end`] takes it.
#[derive(Debug, Default)]
struct Run {
    error: Shown,
    /// Empty when the run shows no note.
    note: String,
}

/// The error that the running kind shows.
#[derive(Debug, Default, PartialEq, Eq)]
enum Shown {
    /// The run showed nothing yet, so the status keeps the end of the last run.
    #[default]
    Nothing,
    /// The run cleared the error last.
    Cleared,
    /// The text of the last error that the run showed, as the class `Retry`.
    Error(String),
}

impl fmt::Debug for Values {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Values")
            .field("counts", &self.counts)
            .field("run", &self.run.borrow())
            .field("staged", &self.staged.get())
            .finish_non_exhaustive()
    }
}

/// The writer of one connector's status channels: the supervisor sets each change of
/// state, and [`Writer::flush`] writes the counts, the error, and the note that the
/// kind shows.
pub(crate) struct Writer {
    session: RefCell<Session>,
    values: Rc<Values>,
    /// The parent of each event of the status.
    span: tracing::Span,
    clock: Clock,
    cancel: cancel::Token,
}

impl Writer {
    /// Opens the writer of the status channels of `connector`, whose kind named
    /// `counts`, as the connector, and gives the status that it writes. `clock` times
    /// the writes. `cancel` is the token of the call: it ends each wait for the home,
    /// and after it [`Self::start`] writes nothing. Each event of the writer has `span`
    /// as its parent.
    pub(crate) async fn open(
        hub: &hub::Hub,
        connector: &Name,
        counts: Vec<Name>,
        span: tracing::Span,
        clock: Clock,
        cancel: cancel::Token,
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
        let (error, note) = (entries[ERROR], entries[NOTE]);
        let form = Variable::of(set.entries()[error].data_type)
            .expect("invariant: `error` is a `String`");
        let empty = form
            .len(&[""])
            .expect("invariant: an empty text has a length");
        let mut series: Box<[_]> = entries
            .iter()
            .map(|&entry| {
                let width = (entry != error && entry != note).then(|| {
                    let width = set.entries()[entry].data_type.width();
                    width.expect("invariant: a status sample but a text has one width")
                });
                (entry, width.unwrap_or(empty))
            })
            .collect();
        series.sort_unstable();
        let text = |entry: usize| Text {
            value: String::new(),
            at: series.partition_point(|&(other, _)| other < entry),
            form,
        };
        let (error, note) = (text(error), text(note));
        let group = set.entries()[entries[0]].group;
        let status = Status::new(counts);
        let session = Session {
            hub: session,
            entries,
            series,
            group,
            stamps: Stamps::default(),
            wrote: Monotonic::default(),
            phase: Phase::Running,
            class: None,
            restarts: 0,
            error,
            note,
            started: false,
            unapplied: None,
            closed: false,
        };
        let writer = Self {
            session: RefCell::new(session),
            values: Rc::clone(&status.0),
            span,
            clock,
            cancel,
        };
        Ok((writer, status))
    }

    /// Writes `state` 0, with one more restart after the first start, and returns
    /// `true`. Returns `false` with no write when `cancel` is cancelled.
    pub(crate) async fn start(&self) -> bool {
        self.applied().await;
        if self.cancel.cancelled() {
            return false;
        }
        self.change(|session| {
            if session.started {
                session.restarts = session.restarts.strict_add(1);
            }
            session.started = true;
            session.phase = Phase::Running;
            let restarts = session.restarts;
            tracing::info!(parent: &self.span, restarts, "running");
        });
        self.values.run.replace(Some(Run::default()));
        true
    }

    /// Writes `state` 3 with the class and the text of a run that ended with `end`,
    /// in place of the error that the run showed. It takes them at the call, so the
    /// caller can drop `end` before it awaits.
    pub(crate) fn end(
        &self,
        end: &Result<(), kind::Error>,
    ) -> impl Future<Output = ()> + use<'_> {
        self.values.run.take();
        let (class, error) = Class::with_text(end);
        let span = &self.span;
        match class {
            None => tracing::info!(parent: span, "ended"),
            Some(class @ Class::Config) => {
                let class = class.name();
                tracing::error!(parent: span, class, error = error.as_str(), "ended");
            }
            Some(class @ (Class::Device | Class::Retry)) => {
                let class = class.name();
                tracing::warn!(parent: span, class, error = error.as_str(), "ended");
            }
        }
        self.set(move |session| {
            session.class = class;
            session.error.set(&mut session.series, error);
            session.note.set(&mut session.series, String::new());
            session.phase = Phase::Ending;
        })
    }

    /// Writes `state` 1 with the next try `span` after the write, and returns the
    /// time of the next try.
    pub(crate) async fn wait(&self, span: Span) -> Monotonic {
        self.applied().await;
        let next = self.clock.now() + span;
        self.change(|session| session.phase = Phase::Waiting(next));
        tracing::info!(parent: &self.span, backoff = %span, "restarting");
        next
    }

    /// Writes `state` 2.
    pub(crate) async fn stop(&self) {
        self.set(|session| session.phase = Phase::Stopped).await;
        tracing::info!(parent: &self.span, "stopped");
    }

    /// Writes the change of state that `change` makes, after [`Self::applied`], so
    /// that no state replaces a state that the home did not apply in normal load.
    async fn set(&self, change: impl FnOnce(&mut Session)) {
        self.applied().await;
        self.change(change);
    }

    /// Waits until the home applied the last change of state, `cancel` is cancelled,
    /// or [`SETTLE`] passed after the first change that the home did not apply.
    async fn applied(&self) {
        let Some(first) = self.session.borrow().unapplied else {
            return;
        };
        let pending = || self.session.borrow().unapplied.is_some();
        self.settle(pending, first + SETTLE).await;
    }

    /// Writes the status while `pending`, at most once each [`PERIOD`] after the last
    /// write, until `cancel` is cancelled or `until`.
    async fn settle(&self, pending: impl Fn() -> bool, until: Monotonic) {
        let writes = async {
            while pending() {
                self.write_staged().await;
            }
        };
        let bound = self.clock.sleep_until(until);
        self.cancel.race(deadline::before(writes, bound)).await;
    }

    /// Writes the change of state that `change` makes.
    fn change(&self, change: impl FnOnce(&mut Session)) {
        let mut session = self.session.borrow_mut();
        change(&mut session);
        let now = self.clock.now();
        session.unapplied.get_or_insert(now);
        session.write(&self.values, &self.span, now);
    }

    /// Polls `run` to its end. Meanwhile it writes the status that a kind staged or
    /// the home did not apply, at most once each [`PERIOD`] after the last write.
    /// Then it writes it the same way until the home applied it, the writer writes no
    /// more, `cancel` is cancelled, or [`SETTLE`] passed after `run` ended. The
    /// supervisor ends `run` at its last change of state, so that bound counts from
    /// that change.
    pub(crate) async fn during<T>(&self, run: impl Future<Output = T>) -> T {
        let output = beside(run, self.flush()).await;
        let until = self.clock.now() + SETTLE;
        self.settle(|| self.values.staged.get(), until).await;
        output
    }

    /// Writes the staged status until the session closed, then never again. A close
    /// while it waits for [`PERIOD`] ends it at the next wake of that wait. It ends
    /// when its call drops it.
    async fn flush(&self) -> Infallible {
        let values = &self.values;
        loop {
            // A close can come in the poll that took the waker, so no waker is kept
            // after it.
            let closed = poll_fn(|cx| {
                if self.session.borrow().closed {
                    return Poll::Ready(true);
                }
                if values.staged.get() {
                    return Poll::Ready(false);
                }
                values.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            })
            .await;
            if closed {
                break;
            }
            self.write_staged().await;
        }
        pending().await
    }

    /// Waits until [`PERIOD`] after the last write, or the session closed, then writes
    /// the status if it is still staged.
    async fn write_staged(&self) {
        // A write by the supervisor while this sleeps moves the next write later.
        loop {
            let (closed, wrote) = {
                let session = self.session.borrow();
                (session.closed, session.wrote)
            };
            let next = wrote + PERIOD;
            if closed || self.clock.now() >= next {
                break;
            }
            self.clock.sleep_until(next).await;
        }
        if self.values.staged.get() {
            let now = self.clock.now();
            self.session
                .borrow_mut()
                .write(&self.values, &self.span, now);
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
    stamps: Stamps,
    /// When the last write was, by the node's clock.
    wrote: Monotonic,
    phase: Phase,
    class: Option<Class>,
    restarts: u64,
    error: Text,
    note: Text,
    /// Set at the first start, after which each start is a restart.
    started: bool,
    /// The time of the first change of state that the home did not apply, while no
    /// frame after it applied and the writer writes more.
    unapplied: Option<Monotonic>,
    /// Set after the hub refused a write as `Removed` or the home failed on disk,
    /// after which the session writes nothing. Each write after it clears `unapplied`
    /// and the staged flag, which ends each wait for the home.
    closed: bool,
}

/// The text of `error` or of `note`, and where and how a frame holds it.
struct Text {
    /// At most [`TEXT_MAX`] bytes.
    value: String,
    /// The index in `Session::series` of its series, one of the two whose length
    /// changes.
    at: usize,
    form: Variable,
}

impl Text {
    /// Sets the text to `value`, and the length of its series in `series`.
    fn set(&mut self, series: &mut [(usize, usize)], value: String) {
        let len = self.form.len(&[&value]);
        series[self.at].1 = len.expect("invariant: at most TEXT_MAX bytes");
        self.value = value;
    }
}

impl Session {
    /// Writes the last value of each status channel. A frame that the home refuses
    /// as `Backwards` is stamped after the stamp the refusal gives and written again,
    /// one time. A frame that the home does not apply, or for which the shard's pool
    /// has no block now, leaves the status staged, so the flush writes it again.
    /// After the session closed, it writes nothing.
    ///
    /// # Panics
    ///
    /// When the draft or the home refuses the frame for a cause that only a defect
    /// gives, which includes `Backwards` on the frame written again and a frame larger
    /// than the largest block of the pool.
    fn write(&mut self, values: &Values, span: &tracing::Span, now: Monotonic) {
        values.staged.set(false);
        if let Some(run) = &*values.run.borrow() {
            match &run.error {
                Shown::Nothing => {}
                Shown::Cleared => {
                    if self.class.is_some() || !self.error.value.is_empty() {
                        tracing::info!(parent: span, "cleared");
                    }
                    self.class = None;
                    if !self.error.value.is_empty() {
                        self.error.set(&mut self.series, String::new());
                    }
                }
                Shown::Error(text) => {
                    if self.class != Some(Class::Retry) || self.error.value != *text {
                        tracing::warn!(parent: span, error = text.as_str(), "error");
                    }
                    self.class = Some(Class::Retry);
                    if self.error.value != *text {
                        self.error.set(&mut self.series, text.clone());
                    }
                }
            }
            if self.note.value != run.note {
                tracing::info!(parent: span, note = run.note.as_str(), "note");
                self.note.set(&mut self.series, run.note.clone());
            }
        }
        if self.closed {
            self.unapplied = None;
            return;
        }
        self.wrote = now;
        let mesh = self.hub.now();
        let stamp = self.stamps.next(mesh);
        // The hub's clock can step while the node's clock does not.
        let next = match self.phase {
            Phase::Waiting(next) => Some(mesh + (next - now)),
            Phase::Running | Phase::Stopped | Phase::Ending => None,
        };
        if let Err(before) = self.send(values, stamp, next) {
            let stamp = self.stamps.after(before);
            if let Err(again) = self.send(values, stamp, next) {
                panic!(
                    "invariant: a status frame stamped after {before}, the last stamp \
                     of its index, is not backwards, but the home gives {again} as last"
                )
            }
        }
        if !values.staged.get() {
            self.unapplied = None;
        }
    }

    /// Writes one frame of the last value of each status channel at `stamp`, with
    /// `next` the time of the next try by the hub's clock, while one waits.
    ///
    /// # Errors
    ///
    /// The stamp that the home gives when it refuses the frame as `Backwards`.
    ///
    /// # Panics
    ///
    /// When the draft or the home refuses the frame for a cause that only a defect
    /// gives, which includes a frame larger than the largest block of the pool.
    fn send(
        &mut self,
        values: &Values,
        stamp: Stamp,
        next: Option<Stamp>,
    ) -> Result<(), Stamp> {
        let mut draft = match self.hub.draft(Form::Raw, &self.series) {
            Ok(draft) => draft,
            Err(frame::Error::Pool(error)) => {
                check_pool(&error);
                values.stage();
                return Ok(());
            }
            Err(error) => panic!("invariant: the series follow the key set: {error}"),
        };
        self.fill(&mut draft, values, stamp, next);
        match self.hub.write(Label::Path(Path::Live), draft) {
            Ok([Outcome::Applied { .. }]) => {}
            Err(Failure::Removed(_) | Failure::Home(home::Error::Disk(_))) => {
                self.closed = true;
                values.waker.take();
            }
            Ok(
                [
                    Outcome::Refused {
                        refusal: Refusal::Order(order::Error::Backwards { before, .. }),
                        ..
                    },
                ],
            ) => return Err(*before),
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
            Ok(
                [
                    Outcome::Refused {
                        refusal: refusal @ (Refusal::Expired | Refusal::Codec { .. }),
                        ..
                    },
                ],
            ) => panic!("the home refuses a status frame: {refusal}"),
            Ok(outcomes) => {
                panic!("invariant: a frame of one group has one outcome: {outcomes:?}")
            }
            Err(Failure::Home(
                error @ (home::Error::Resend | home::Error::Full | home::Error::Large),
            )) => panic!("the home refuses a status frame: {error}"),
        }
        Ok(())
    }

    /// Fills `draft` with the last value of each status channel at `stamp`, with
    /// `next` the time of the next try by the hub's clock, while one waits.
    fn fill(
        &self,
        draft: &mut Draft,
        values: &Values,
        stamp: Stamp,
        next: Option<Stamp>,
    ) {
        let backoff = next.map_or(Span::ZERO, |next| (next - stamp).max(Span::ZERO));
        let supervisor = [
            u64::from(self.phase.state().code()).to_le_bytes(),
            u64::from(Class::code(self.class)).to_le_bytes(),
            self.restarts.to_le_bytes(),
            backoff.nanos().to_le_bytes(),
        ];
        let counts = values
            .counts
            .iter()
            .map(|(_, count)| count.get().to_le_bytes());
        let mut samples = supervisor.into_iter().chain(counts);
        for (i, &entry) in self.entries.iter().enumerate() {
            let bytes = draft
                .series_mut(entry)
                .expect("invariant: each entry has a series");
            if i == 0 {
                bytes.copy_from_slice(&stamp.nanos().to_le_bytes());
                continue;
            }
            if let Some(text) = match i {
                ERROR => Some(&self.error),
                NOTE => Some(&self.note),
                _ => None,
            } {
                text.form.write(&[&text.value], bytes);
                continue;
            }
            let sample = samples.next().expect("invariant: a sample per channel");
            let len = bytes.len();
            bytes.copy_from_slice(&sample[..len]);
        }
        draft.set_count(self.group, 1);
    }
}

/// `text` when it fits in [`TEXT_MAX`] bytes. Else its head and its tail, cut at char
/// boundaries and joined by [`MARK`], so that an error with a long subject keeps its
/// reason at the end. No spare capacity.
fn cut(text: &str) -> String {
    if text.len() <= TEXT_MAX {
        return text.to_owned();
    }
    let half = (TEXT_MAX - MARK.len()) / 2;
    let head = &text[..text.floor_char_boundary(half)];
    let tail = &text[text.ceil_char_boundary(text.len() - half)..];
    [head, MARK, tail].concat()
}

/// Polls `run` to its end, and `side` after it while `run` is pending, so `side` sees
/// what `run` did at the same instant. `side` has a waker of its own, and is polled
/// only after a wake of it, so a poll of `run` alone costs one atomic load.
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
        if let Poll::Ready(out) = run.as_mut().poll(cx) {
            return Poll::Ready(out);
        }
        // A wake that the load misses wakes the task again.
        if woke.woken.load(Ordering::Relaxed)
            && woke.woken.swap(false, Ordering::AcqRel)
        {
            let side = side.as_mut().poll(&mut task::Context::from_waker(&waker));
            if let Poll::Ready(never) = side {
                match never {}
            }
        }
        Poll::Pending
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

/// Checks that the pool can give a status frame later.
///
/// # Panics
///
/// When no block of the pool holds a status frame.
fn check_pool(error: &block::Error) {
    match error {
        block::Error::Exhausted { .. } | block::Error::Refused { .. } => {}
        block::Error::TooLarge { .. } => {
            panic!(
                "invariant: a status frame fits the largest block of the pool: {error}"
            )
        }
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use proptest::prelude::*;

    use super::*;
    use crate::testing::create_status;

    #[test]
    fn defines_the_status_channels_of_one_connector() {
        let (a, b) = (name("site.a"), name("site.b"));
        let counts = [name("in")];
        let mine = create_status(&a, &counts, types::channel::Key::from_u128(100));
        let theirs = create_status(&b, &counts, types::channel::Key::from_u128(200));
        let definitions: BTreeMap<_, _> = mine.iter().chain(&theirs).cloned().collect();
        let mut expected: Vec<_> = mine
            .iter()
            .map(|(name, definition)| match definition {
                Definition::Channel(channel) => (name, channel),
                _ => unreachable!("a status definition is a channel"),
            })
            .collect();
        expected[1..].sort_by_key(|(name, _)| *name);
        assert_eq!(defined(&definitions, &a), expected);
    }

    #[test]
    #[should_panic(expected = "defines the status index `site.a.status.time`")]
    fn panics_with_no_status_index() {
        drop(defined(&BTreeMap::new(), &name("site.a")));
    }

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
            (
                name("plant.modbus.status.backoff"),
                Type::Scalar(Scalar::Span),
            ),
            (name("plant.modbus.status.error"), Type::String),
            (name("plant.modbus.status.note"), Type::String),
            (name("plant.modbus.status.samples"), u64),
            (name("plant.modbus.status.errors"), u64),
        ];
        assert_eq!(time, name("plant.modbus.status.time"));
        assert_eq!(channels, want);
    }

    proptest! {
        /// `cut` is pure, so this reaches each length and char width, which a run
        /// through the supervisor does not. A constant spare capacity cancels in each
        /// difference of held bytes that a caller can measure, so only this one sees
        /// it.
        #[test]
        fn cuts_the_middle_of_a_text_that_does_not_fit(
            text in "(a|é|€|😀){0,1100}",
        ) {
            let got = cut(&text);
            prop_assert!(got.len() <= TEXT_MAX);
            prop_assert_eq!(got.capacity(), got.len());
            if text.len() <= TEXT_MAX {
                prop_assert_eq!(got, text);
            } else {
                let half = (TEXT_MAX - MARK.len()) / 2;
                let (head, tail) = got.split_once(MARK).expect("a mark");
                prop_assert!(text.starts_with(head) && text.ends_with(tail));
                let next = text[head.len()..].chars().next().expect("a cut char");
                prop_assert!(head.len() + next.len_utf8() > half);
                let last = text[..text.len() - tail.len()].chars().next_back();
                let last = last.expect("a cut char");
                prop_assert!(tail.len() + last.len_utf8() > half);
            }
        }
    }

    /// A constant spare capacity cancels in each difference of held bytes that a caller
    /// can measure, so only this one sees it.
    #[test]
    fn gives_the_text_of_an_error_with_no_spare_capacity() {
        let device = Err(kind::Error::Device("busy".into()));
        let (class, text) = Class::with_text(&device);
        assert_eq!((class, text.as_str()), (Some(Class::Device), "busy"));
        assert_eq!(text.capacity(), text.len());
    }

    /// A constant spare capacity of the value cancels in each difference of held bytes
    /// that a caller can measure, so only this one sees it.
    #[test]
    fn sets_a_text_with_no_spare_capacity() {
        let form = Variable::of(Type::String).expect("a `String` has a form");
        let len = form.len(&["busy"]).expect("a short text has a length");
        let value = String::new();
        let mut text = Text { value, at: 1, form };
        let mut series = [(0, 0), (0, 0)];
        text.set(&mut series, "busy".to_owned());
        assert_eq!((text.value.as_str(), text.value.capacity()), ("busy", 4));
        assert_eq!(series, [(0, 0), (0, len)]);
    }

    #[test]
    fn debugs_each_value_but_the_waker() {
        let status = Status::new(vec![name("samples")]);
        status.count("samples").set(3);
        let want = "Status(Values { counts: \
                    [(Name(\"samples\"), Cell { value: 3 })], run: None, staged: \
                    true, .. })";
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

    /// No test of a caller makes the system refuse memory to the pool, so only this
    /// test sees `Refused` moved to the arm that panics.
    #[test]
    fn check_pool_accepts_a_pool_with_no_block_now() {
        check_pool(&block::Error::Exhausted {
            requested: 128,
            available: 64,
        });
        check_pool(&block::Error::Refused { requested: 128 });
    }

    #[test]
    #[should_panic(
        expected = "invariant: a status frame fits the largest block of the pool: \
                    block of 128 bytes is above the largest block of 64 bytes"
    )]
    fn panics_when_no_block_of_the_pool_holds_a_status_frame() {
        check_pool(&block::Error::TooLarge {
            requested: 128,
            largest: 64,
        });
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use std::future::{pending, poll_fn};
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{self, Poll, Wake, Waker};

    use loom::sync::Mutex;
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::thread;

    use super::beside;

    /// A task waker that records its wake.
    #[derive(Default)]
    struct Task(AtomicBool);

    impl Wake for Task {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// A wake of the side from another thread, while a poll with a new task waker
    /// runs, reaches that poll or wakes the new task.
    #[test]
    fn a_wake_of_the_side_from_another_thread_is_not_lost() {
        loom::model(|| {
            let (slot, polls) =
                (Arc::new(Mutex::new(None)), Arc::new(AtomicUsize::new(0)));
            let (stored, counted) = (Arc::clone(&slot), Arc::clone(&polls));
            let side = poll_fn(move |cx| {
                counted.fetch_add(1, Ordering::Relaxed);
                *stored.lock().expect("no panic under the lock") =
                    Some(cx.waker().clone());
                Poll::Pending
            });
            let mut both = pin!(beside(pending::<()>(), side));
            let (first, second) =
                (Arc::new(Task::default()), Arc::new(Task::default()));
            let first_waker = Waker::from(Arc::clone(&first));
            let pending = both
                .as_mut()
                .poll(&mut task::Context::from_waker(&first_waker));
            assert!(pending.is_pending());
            let side_waker: Waker = slot
                .lock()
                .expect("no panic under the lock")
                .take()
                .expect("the side keeps a waker");
            let waking = thread::spawn(move || side_waker.wake());
            let second_waker = Waker::from(Arc::clone(&second));
            let pending = both
                .as_mut()
                .poll(&mut task::Context::from_waker(&second_waker));
            assert!(pending.is_pending());
            waking.join().expect("no panic in the wake");
            assert!(
                polls.load(Ordering::Relaxed) == 2 || second.0.load(Ordering::Acquire),
                "the second poll sees the wake, or the wake wakes the second task"
            );
        });
    }
}
