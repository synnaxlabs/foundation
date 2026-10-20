//! The subscribed run of the `opcua` kind: it watches each node, and writes each
//! change at the mesh time of its timestamp, through a fit of the server clock.

use connector::kind::{Context, Error};
use connector::stamp::{Fit, Fitted};
use document::diagnostic::{Code, Diagnostic};
use types::time::{Interval, Monotonic, Span, Stamp};

use super::{Call, Failure, Frames, Why, failed, too_large};
use crate::client::Client;
use crate::client::subscription::Change;
use crate::config::{Config, Read};
use crate::ffi::Status;
use crate::node::Node;
use crate::value::{Fault, Value};

#[cfg(test)]
mod tests;

const NO_TIMESTAMP: Code = Code::new("opcua.no-timestamp");
const LARGE_WATCH: Code = Code::new("opcua.large-watch");

/// The span from one read of the current time of the server to the next.
const READ: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The offset of the map of [`device`]: 2^62 ns, about 146 years.
const OFFSET: i64 = 1 << 62;

/// Subscribes to each node of the reads of `ctx` at the rate of `ctx`, and writes each
/// change until the cancel. It reads the current time of the server first, then each
/// [`READ`], and at once when a server timestamp disagrees with the fit or goes back.
/// Gives the status when the server answers the subscribe with
/// `BadServiceUnsupported` or `BadNotImplemented`, and `None` at the cancel.
///
/// # Errors
///
/// [`Error::Device`] when a read of the current time or the subscribe fails, when
/// the server does not watch a node, and when the subscription ends.
/// [`Error::Config`] with `opcua.no-timestamp` for a Good change with no timestamp that
/// maps to mesh time, and with `opcua.large-watch` for a node whose
/// `CreateMonitoredItems` call alone is past one message of the server. The errors of
/// [`Frames::write`].
pub(super) async fn run(
    ctx: &Context<Config>,
    client: &mut Client,
    frames: &mut Frames<'_>,
) -> Result<Option<Status>, Error> {
    let reads = &ctx.config().reads;
    let mut server = Server {
        fit: Fit::default(),
        mesh: frames.writer.time(),
    };
    let Some(round_trip) = ctx.cancel().race(server.read(client)).await else {
        return Ok(None);
    };
    let nodes: Vec<Node> = reads.iter().map(|read| read.node.clone()).collect();
    let sampling = ctx.config().rate.span(1);
    let subscribe = client.subscribe(&nodes, sampling, round_trip?);
    let revised = match ctx.cancel().race(subscribe).await {
        None => return Ok(None),
        Some(Err(status)) if unsupported(status) => return Ok(Some(status)),
        Some(revised) => {
            revised.map_err(|status| failed(client.url(), Call::Subscribe, status))?
        }
    };
    let mut refused = revised.items.iter().filter_map(|item| item.as_ref().err());
    if let Some(&status) = refused.find(|&&status| unsupported(status)) {
        return Ok(Some(status));
    }
    if let Some(large) = revised.large.first() {
        return Err(too_large(
            LARGE_WATCH,
            "CreateMonitoredItems call",
            reads,
            large,
        ));
    }
    for (read, item) in reads.iter().zip(&revised.items) {
        if let &Err(status) = item {
            return Err(super::device(Failure::Unwatched {
                node: read.node.to_string(),
                channel: read.channel.to_string(),
                status,
            }));
        }
    }
    ctx.clear_error();
    let missed = ctx.count("missed");
    let mut drive = Drive::new(reads.len());
    let mut due = ctx.clock().now() + READ;
    loop {
        let until = ctx.clock().sleep_until(due);
        let Some(changes) = ctx.cancel().race(client.changes(until)).await else {
            break;
        };
        let changes = match changes {
            Ok(changes) => changes,
            Err(status) => {
                let address = client.url().to_string_lossy().into_owned();
                return Err(super::device(Failure::Ended { address, status }));
            }
        };
        let arrived = server.mesh.now();
        let unsure = drive.take(changes, &mut server.fit, arrived, reads)?;
        missed.add(drive.overflows);
        if unsure || ctx.clock().now() >= due {
            let Some(read) = ctx.cancel().race(server.read(client)).await else {
                break;
            };
            read?;
            due = ctx.clock().now() + READ;
        }
        drive.write(frames, &server.fit, reads)?;
    }
    Ok(None)
}

/// Takes and writes `count` drives of the reads of `ctx` with no server, each a change
/// of each of the first `moved` reads with the value 1 in the sample type of its
/// channel (`true` for `bool`), both timestamps 0.5 ms before the drive and `spread`
/// before those of the read before it, and each drive
/// 1 ms of its clock after the last, through a fit of a server clock that reads mesh
/// time exactly. Gives what `measure` gives for the take and the write of each drive.
/// `measure` calls the work that it gets once. For benchmarks.
///
/// # Errors
///
/// As [`Kind::run`](connector::kind::Kind::run), with no connect or subscribe.
///
/// # Panics
///
/// As [`writes`](super::writes).
#[cfg(feature = "sim")]
pub async fn drives(
    ctx: &Context<Config>,
    count: usize,
    moved: usize,
    spread: Span,
    mut measure: impl FnMut(&mut dyn FnMut()) -> u64,
) -> Result<Vec<u64>, Error> {
    let mut frames = Frames::open(ctx)
        .await?
        .expect("invariant: a benchmark run is not cancelled before its writer opens");
    let reads = &ctx.config().reads;
    let mesh = frames.writer.time();
    let mut fit = Fit::default();
    let now = mesh.now();
    _ = fit.between(device(now).expect("a mesh time in the map"), now, now);
    let mut changes: Vec<Change> = (frames.types.iter().take(moved).enumerate())
        .map(|(index, &scalar)| Change {
            index,
            value: Ok(Value::one(scalar)),
            source: None,
            server: None,
            overflow: false,
        })
        .collect();
    let mut drive = Drive::new(reads.len());
    let mut measures = Vec::with_capacity(count);
    for _ in 0..count {
        ctx.clock().sleep(Span::from_nanos(1_000_000)).await;
        let arrived = mesh.now();
        let mut at = arrived - Span::from_nanos(500_000);
        for change in &mut changes {
            (change.source, change.server) = (Some(at), Some(at));
            at = at - spread;
        }
        let mut written = Ok(());
        measures.push(measure(&mut || {
            written = drive
                .take(&changes, &mut fit, arrived, reads)
                .and_then(|_| drive.write(&mut frames, &fit, reads));
        }));
        written?;
    }
    Ok(measures)
}

/// The fit of the server clock to the mesh time of the writer.
struct Server {
    fit: Fit,
    mesh: hub::Time,
}

impl Server {
    /// Reads the current time of the server, and adds it to the fit between the mesh
    /// times before and after the read. Gives the round trip of the read.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the read fails, or gives a time before the map of
    /// [`device`].
    async fn read(&mut self, client: &mut Client) -> Result<Span, Error> {
        let sent = self.mesh.now();
        let time = client.time().await;
        let answered = self.mesh.now();
        let time = time.and_then(|time| device(time).ok_or(Status::BAD_OUT_OF_RANGE));
        let time = time.map_err(|status| failed(client.url(), Call::Read, status))?;
        _ = self.fit.between(time, sent, answered);
        Ok(answered - sent)
    }
}

/// Whether `status` says that the server has no subscription service.
fn unsupported(status: Status) -> bool {
    status == Status::BAD_SERVICE_UNSUPPORTED || status == Status::BAD_NOT_IMPLEMENTED
}

/// Gives the device time of the server timestamp `time`, as a [`Fit`] takes it:
/// `time + 2^62` ns, or `None` before `-2^62` ns.
pub(super) fn device(time: Stamp) -> Option<Monotonic> {
    let time = i128::from(time.nanos()) + i128::from(OFFSET);
    u64::try_from(time).ok().map(Monotonic)
}

/// The changes of one drive of the subscription. It keeps its buffers from drive to
/// drive, so a drive allocates nothing once they hold the largest drive.
struct Drive {
    entries: Vec<Entry>,
    /// The entry of the frame that is filled for each read, in order.
    slots: Vec<Option<usize>>,
    /// The device time of the next value of each read, in the order of `entries`.
    next: Vec<Option<Monotonic>>,
    /// The device time of the last entry of each read, in the order of changes.
    last: Vec<Option<Monotonic>>,
    /// Whether the last change of each read is Bad or Uncertain.
    faulted: Vec<bool>,
    /// The read of each change of the last drive, whose `last` and `faulted` the next
    /// drive clears.
    touched: Vec<usize>,
    /// The device time of the newest server timestamp of the last drive with one.
    newest: Option<Monotonic>,
    /// The count of changes with an overflow.
    overflows: u64,
    /// The counts of Bad and of Uncertain changes, which make no entry.
    bad: u64,
    uncertain: u64,
}

/// A change of a drive.
struct Entry {
    /// The index of the read of the node.
    index: usize,
    value: Result<Value, Fault>,
    /// The device time of the value: its source timestamp, or its server timestamp
    /// when that is earlier or the source timestamp is absent.
    at: Monotonic,
    /// The device time of the server timestamp.
    server: Option<Monotonic>,
    /// The position of the change in the drive.
    order: usize,
    /// The mesh time until which the value holds at least.
    held: Stamp,
}

impl Drive {
    fn new(reads: usize) -> Self {
        Self {
            entries: Vec::new(),
            slots: vec![None; reads],
            next: vec![None; reads],
            last: vec![None; reads],
            faulted: vec![false; reads],
            touched: Vec::new(),
            newest: None,
            overflows: 0,
            bad: 0,
            uncertain: 0,
        }
    }

    /// Takes `changes`, which arrived before mesh time `arrived`, and adds each server
    /// timestamp to `fit`. Gives whether `fit` needs a read of the current time: one
    /// of them restarted it, or the newest of them is older than the newest of the
    /// last drive with one, as from a clock that went back. A Bad or Uncertain change
    /// makes no entry and adds to its count, and [`Drive::write`] clears a note of its
    /// read when it is the last change of that read. The device time of an entry is at
    /// least that of the entry before it of its read, so each read keeps the order of
    /// its changes.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with `opcua.no-timestamp` for an entry with neither
    /// timestamp.
    fn take(
        &mut self,
        changes: &[Change],
        fit: &mut Fit,
        arrived: Stamp,
        reads: &[Read],
    ) -> Result<bool, Error> {
        self.entries.clear();
        for index in self.touched.drain(..) {
            (self.last[index], self.faulted[index]) = (None, false);
        }
        (self.overflows, self.bad, self.uncertain) = (0, 0, 0);
        let (mut unsure, mut newest) = (false, None);
        for (order, change) in changes.iter().enumerate() {
            let server = timestamp(change.server);
            if let Some(server) = server {
                unsure |= fit.before(server, arrived) == Fitted::Restarted;
                newest = newest.max(Some(server));
            }
            self.overflows += u64::from(change.overflow);
            self.touched.push(change.index);
            let faulted = &mut self.faulted[change.index];
            *faulted = true;
            if let Err(fault) = &change.value {
                match Why::fault(*fault) {
                    Why::Bad => {
                        self.bad += 1;
                        continue;
                    }
                    Why::Uncertain => {
                        self.uncertain += 1;
                        continue;
                    }
                    Why::Mistyped(_) => {}
                }
            }
            *faulted = false;
            let at = match (timestamp(change.source), server) {
                (Some(source), Some(server)) => source.min(server),
                (source, server) => {
                    let Some(at) = source.or(server) else {
                        return Err(no_timestamp(&reads[change.index]));
                    };
                    at
                }
            };
            // A clock that steps back inside the drive gives an earlier device time.
            let at = self.last[change.index].map_or(at, |last| at.max(last));
            self.last[change.index] = Some(at);
            self.entries.push(Entry {
                index: change.index,
                value: change.value,
                at,
                server,
                order,
                held: Stamp::EPOCH,
            });
        }
        if newest.is_some() {
            unsure |= newest < self.newest;
            self.newest = newest;
        }
        Ok(unsure)
    }

    /// Sorts the entries by device time, then read, then arrival, and sets the hold
    /// of each: the mesh time that `fit` gives the earlier of its server timestamp
    /// and the device time of the next value of its read, at its earliest. With
    /// neither, or one that `fit` cannot place, the value holds for no time.
    fn order(&mut self, fit: &Fit) {
        let Self { entries, next, .. } = self;
        entries.sort_unstable_by_key(|entry| (entry.at, entry.index, entry.order));
        for entry in entries.iter() {
            next[entry.index] = None;
        }
        let mut last: Option<(Monotonic, Stamp)> = None;
        for entry in entries.iter_mut().rev() {
            let later = next[entry.index].replace(entry.at);
            let held = match (later, entry.server) {
                (Some(later), Some(server)) => Some(later.min(server)),
                (later, server) => later.or(server),
            };
            // The changes of one device time often share a hold: place it once.
            entry.held = if let Some((at, stamp)) = last
                && held == Some(at)
            {
                stamp
            } else {
                let mesh = held.and_then(|held| fit.mesh(held));
                let stamp =
                    mesh.map_or(Stamp::from_nanos(i64::MIN), |held| held.earliest);
                last = held.map(|held| (held, stamp));
                stamp
            };
        }
    }

    /// Writes the entries in the order of [`Drive::order`], with one frame for each
    /// device time, at the mesh time that `fit` gives it. A second value of a read at
    /// one device time starts a new frame. When `fit` cannot place a device time, the
    /// frame takes the mesh time of the server timestamp of its first value.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with `opcua.no-timestamp` when `fit` can place neither. As
    /// [`Frames::write`].
    fn write(
        &mut self,
        frames: &mut Frames<'_>,
        fit: &Fit,
        reads: &[Read],
    ) -> Result<(), Error> {
        frames.left.add(self.bad, self.uncertain, 0);
        self.order(fit);
        let Self { entries, slots, .. } = self;
        let mut start = 0;
        while let Some(first) = entries.get(start) {
            let mut held = Stamp::from_nanos(i64::MAX);
            let mut end = start;
            while let Some(entry) = entries.get(end)
                && entry.at == first.at
                && slots[entry.index].is_none()
            {
                slots[entry.index] = Some(end);
                held = held.min(entry.held);
                end += 1;
            }
            let time = first.time(fit, reads)?;
            // The error of the stamp is not written yet (#2627).
            let (stamp, _) = frames.writer.sample(time, held);
            let values = slots
                .iter_mut()
                .map(|slot| Some(&entries[slot.take()?].value));
            frames.write(values, stamp)?;
            start = end;
        }
        frames.faulted(&self.faulted);
        Ok(())
    }
}

impl Entry {
    /// Gives the mesh time that `fit` gives the device time of the entry, or else its
    /// server timestamp.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with `opcua.no-timestamp` when `fit` places neither.
    fn time(&self, fit: &Fit, reads: &[Read]) -> Result<Interval, Error> {
        (fit.mesh(self.at))
            .or_else(|| self.server.and_then(|server| fit.mesh(server)))
            .ok_or_else(|| no_timestamp(&reads[self.index]))
    }
}

/// Gives the device time of the timestamp `time`, or `None` when it is absent, 0, or
/// before the map of [`device`].
fn timestamp(time: Option<Stamp>) -> Option<Monotonic> {
    time.filter(|&time| time != Stamp::EPOCH).and_then(device)
}

/// The error of a change of the node of `read` with no timestamp that maps to mesh
/// time.
#[cold]
fn no_timestamp(read: &Read) -> Error {
    Error::Config(vec![Diagnostic::new(
        NO_TIMESTAMP,
        None,
        format!(
            "the server gives a value of the node `{}` of the channel `{}` with no \
             timestamp that maps to mesh time, so the subscription cannot place it",
            read.node, read.channel,
        ),
        "Set `polled = true` to read each node at the rate".into(),
    )])
}
