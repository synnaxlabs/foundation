//! The status channels of a connector, `<connector>.status.<name>`, on one index of
//! their own, and their writes.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};

use env::clock::Clock;
use types::authority::Authority;
use types::frame::{Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Monotonic, Span, Stamp};

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
/// # Panics
///
/// When the name of a status channel is longer than [`Name::MAX_BYTES`].
#[must_use]
pub fn channels(connector: &Name, counts: &[Name]) -> (Name, Vec<(Name, Type)>) {
    let status = |last: &str| -> Name {
        let text = format!("{connector}.status.{last}");
        text.parse().unwrap_or_else(|error| {
            panic!("the status channel `{text}` is not a name: {error}")
        })
    };
    let supervisor = SUPERVISOR.iter().map(|&(name, sample)| (name, sample));
    let counts = counts
        .iter()
        .map(|count| (count.as_str(), Type::Scalar(Scalar::U64)));
    let channels = supervisor
        .chain(counts)
        .map(|(last, sample)| (status(last), sample))
        .collect();
    (status(TIME), channels)
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
            state: Cell::new(State::Running),
            class: Cell::new(Class::None),
            restarts: Cell::new(0),
            counts: counts.into_iter().map(|n| (n, Cell::new(0))).collect(),
            staged: Cell::new(false),
            waker: Cell::new(None),
        }))
    }

    /// The same status, for one more holder.
    pub(crate) fn share(&self) -> Self {
        Self(Rc::clone(&self.0))
    }

    /// The count `name`, to set from the kind's data path.
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

/// One count of a kind, `<connector>.status.<count>`.
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

/// The last value of each status channel, and whether a count changed since the
/// last write.
struct Values {
    state: Cell<State>,
    class: Cell<Class>,
    restarts: Cell<u64>,
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
            .field("state", &self.state.get())
            .field("class", &self.class.get())
            .field("restarts", &self.restarts.get())
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
    /// Set at the first start, after which each start is a restart.
    started: Cell<bool>,
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
        let (time, channels) = channels(connector, &counts);
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
            removed: false,
        };
        let writer = Self {
            session: RefCell::new(session),
            values: Rc::clone(&status.0),
            clock,
            started: Cell::new(false),
        };
        Ok((writer, status))
    }

    /// Writes `state` 0, with one more restart after the first start.
    pub(crate) fn start(&self) {
        if self.started.replace(true) {
            let restarts = &self.values.restarts;
            restarts.set(restarts.get().strict_add(1));
        }
        self.set(State::Running);
    }

    /// Writes `state` 3 with the class of a run that ended with `end`.
    pub(crate) fn end(&self, end: &Result<(), kind::Error>) {
        self.values.class.set(Class::of(end));
        self.set(State::Ending);
    }

    /// Writes `state` 1.
    pub(crate) fn wait(&self) {
        self.set(State::Waiting);
    }

    /// Writes `state` 2.
    pub(crate) fn stop(&self) {
        self.set(State::Stopped);
    }

    fn set(&self, state: State) {
        self.values.state.set(state);
        self.write();
    }

    fn write(&self) {
        let now = self.clock.now();
        self.session.borrow_mut().write(&self.values, now);
    }

    /// Writes the status that a kind staged or the home did not apply, at most once
    /// each [`PERIOD`] after the last write. Never returns.
    #[expect(
        clippy::infinite_loop,
        reason = "the flush ends when its call drops it"
    )]
    pub(crate) async fn flush(&self) -> Infallible {
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
            // A write by the supervisor while this sleeps moves the next write later.
            loop {
                let next = self.session.borrow().wrote + PERIOD;
                if self.clock.now() >= next {
                    break;
                }
                self.clock.sleep_until(next).await;
            }
            if values.staged.get() {
                self.write();
            }
        }
    }
}

/// The hub's writer session of the status channels.
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
    /// Set once a status channel is removed: the writer writes no more.
    removed: bool,
}

impl Session {
    /// Writes the last value of each status channel. A frame that the home does not
    /// apply leaves the status staged, so the flush writes it again.
    fn write(&mut self, values: &Values, now: Monotonic) {
        values.staged.set(false);
        self.wrote = now;
        if self.removed {
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
            u64::from(values.state.get() as u8),
            u64::from(values.class.get() as u8),
            values.restarts.get(),
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
        let applied = match self.hub.write(Label::Path(Path::Live), draft) {
            Ok(outcomes) => matches!(outcomes, [hub::home::Outcome::Applied { .. }]),
            Err(hub::writer::Failure::Home(_)) => false,
            Err(hub::writer::Failure::Removed(_)) => {
                self.removed = true;
                true
            }
        };
        if !applied {
            values.stage();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        text.parse().expect("a valid name")
    }

    #[test]
    fn gives_the_index_then_the_supervisor_channels_then_the_counts() {
        let counts = [name("samples"), name("errors")];
        let (time, channels) = channels(&name("plant.modbus"), &counts);
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
        let want = "Status(Values { state: Running, class: None, restarts: 0, counts: \
                    [(Name(\"samples\"), Cell { value: 3 })], staged: true, .. })";
        assert_eq!(format!("{status:?}"), want);
    }

    #[test]
    fn panics_on_a_status_channel_longer_than_a_name() {
        let connector = "c".repeat(245);
        let connector = name(&connector);
        let panic = std::panic::catch_unwind(|| drop(channels(&connector, &[])));
        let panic = panic.expect_err("a status name over the limit panics");
        let message = panic.downcast::<String>().expect("a formatted message");
        let want = format!(
            "the status channel `{connector}.status.state` is not a name: a name or \
             pattern is 258 bytes long, more than the limit of 255 bytes"
        );
        assert_eq!(*message, want);
    }
}
