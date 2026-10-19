//! The subscribed run of the `opcua` kind: it watches each node, and writes each
//! change at the mesh time of its timestamp, through a fit of the server clock.

use connector::kind::{Context, Error};
use connector::stamp::{Fit, Fitted};
use document::diagnostic::{Code, Diagnostic};
use types::time::{Interval, Monotonic, Span, Stamp};

use super::{Call, Failure, Frames, device as failure, failed};
use crate::client::Client;
use crate::client::subscription::Change;
use crate::config::{Config, Read};
use crate::ffi::Status;
use crate::node::Node;
use crate::value::{Fault, Value};

#[cfg(test)]
mod tests;

const NO_TIMESTAMP: Code = Code::new("opcua.no-timestamp");

/// The span from one read of the current time of the server to the next.
const READ: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The offset of the map of [`device`]: 2^62 ns, about 146 years.
const OFFSET: i64 = 1 << 62;

/// Subscribes to each node of the reads of `ctx` at the rate of `ctx`, and writes
/// each change until the cancel. It reads the current time of the server first, then
/// each [`READ`], and at once when a server timestamp disagrees with the fit.
///
/// # Errors
///
/// [`Error::Device`] when a read of the current time or the subscribe fails, when
/// the server does not watch a node, and when the subscription ends.
/// [`Error::Config`] with `opcua.no-timestamp` for a change with no timestamp. The
/// errors of [`Frames::write`].
pub(super) async fn run(
    ctx: &Context<Config>,
    client: &mut Client,
    frames: &mut Frames<'_>,
) -> Result<(), Error> {
    let reads = &ctx.config().reads;
    let mut clock = Clock {
        fit: Fit::default(),
        mesh: frames.writer.time(),
    };
    clock.read(client).await?;
    let nodes: Vec<Node> = reads.iter().map(|read| read.node.clone()).collect();
    let revised = client.subscribe(&nodes, ctx.config().rate.span(1)).await;
    let revised =
        revised.map_err(|status| failed(client.url(), Call::Subscribe, status))?;
    for (read, item) in reads.iter().zip(&revised.items) {
        if let &Err(status) = item {
            return Err(failure(Failure::Unwatched {
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
                return Err(failure(Failure::Ended { address, status }));
            }
        };
        let arrived = clock.mesh.now();
        let fitted = drive.take(changes, &mut clock.fit, arrived, reads)?;
        missed.add(drive.overflows);
        if fitted == Fitted::Restarted || ctx.clock().now() >= due {
            clock.read(client).await?;
            due = ctx.clock().now() + READ;
        }
        drive.write(frames, &clock.fit)?;
    }
    Ok(())
}

/// The fit of the server clock to the mesh time of the writer.
struct Clock {
    fit: Fit,
    mesh: hub::Time,
}

impl Clock {
    /// Reads the current time of the server, and adds it to the fit between the mesh
    /// times before and after the read.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the read fails, or gives a time before the map of
    /// [`device`].
    async fn read(&mut self, client: &mut Client) -> Result<(), Error> {
        let sent = self.mesh.now();
        let time = client.time().await;
        let answered = self.mesh.now();
        let time = time.and_then(|time| device(time).ok_or(Status::BAD_OUT_OF_RANGE));
        let time = time.map_err(|status| failed(client.url(), Call::Read, status))?;
        _ = self.fit.between(time, sent, answered);
        Ok(())
    }
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
    /// The mesh time after the drive.
    arrived: Stamp,
    /// The count of changes with an overflow.
    overflows: u64,
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
            arrived: Stamp::EPOCH,
            overflows: 0,
        }
    }

    /// Takes `changes`, which arrived before mesh time `arrived`, and adds each server
    /// timestamp to `fit`. Gives [`Fitted::Restarted`] when one of them restarted it.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with `opcua.no-timestamp` for a change with neither
    /// timestamp.
    fn take(
        &mut self,
        changes: &[Change],
        fit: &mut Fit,
        arrived: Stamp,
        reads: &[Read],
    ) -> Result<Fitted, Error> {
        self.entries.clear();
        self.arrived = arrived;
        self.overflows = 0;
        let mut fitted = Fitted::Kept;
        for (order, change) in changes.iter().enumerate() {
            let server = timestamp(change.server);
            if let Some(server) = server
                && fit.before(server, arrived) == Fitted::Restarted
            {
                fitted = Fitted::Restarted;
            }
            let at = match (timestamp(change.source), server) {
                (Some(source), Some(server)) => source.min(server),
                (source, server) => {
                    let Some(at) = source.or(server) else {
                        return Err(no_timestamp(&reads[change.index]));
                    };
                    at
                }
            };
            self.overflows += u64::from(change.overflow);
            self.entries.push(Entry {
                index: change.index,
                value: change.value,
                at,
                server,
                order,
                held: Stamp::EPOCH,
            });
        }
        Ok(fitted)
    }

    /// Sorts the entries by device time, then read, then arrival, and sets the hold
    /// of each: the mesh time that `fit` gives the earlier of its server timestamp
    /// and the device time of the next value of its read, at its earliest. With
    /// neither, or one that `fit` cannot place, the value holds for no time.
    fn order(&mut self, fit: &Fit) {
        let Self { entries, next, .. } = self;
        entries.sort_unstable_by_key(|entry| (entry.at, entry.index, entry.order));
        next.fill(None);
        for entry in entries.iter_mut().rev() {
            let later = next[entry.index].replace(entry.at);
            let held = match (later, entry.server) {
                (Some(later), Some(server)) => Some(later.min(server)),
                (later, server) => later.or(server),
            };
            let held = held.and_then(|held| fit.mesh(held));
            entry.held = held.map_or(Stamp::from_nanos(i64::MIN), |held| held.earliest);
        }
    }

    /// Writes the entries in the order of [`Drive::order`], with one frame for each
    /// device time, at the mesh time that `fit` gives it. A second value of a read at
    /// one device time starts a new frame. When `fit` cannot place a device time, the
    /// frame takes the mesh time of the server timestamp of its first value, or the
    /// mesh time after the drive.
    ///
    /// # Errors
    ///
    /// As [`Frames::write`].
    fn write(&mut self, frames: &mut Frames<'_>, fit: &Fit) -> Result<(), Error> {
        self.order(fit);
        let Self {
            entries,
            slots,
            arrived,
            ..
        } = self;
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
            let time = (fit.mesh(first.at))
                .or_else(|| first.server.and_then(|server| fit.mesh(server)))
                .unwrap_or(Interval {
                    earliest: *arrived,
                    latest: *arrived,
                });
            // The error of the stamp is not written yet (#2627).
            let (stamp, _) = frames.writer.sample(time, held);
            let values = slots
                .iter_mut()
                .map(|slot| Some(&entries[slot.take()?].value));
            frames.write(values, stamp)?;
            start = end;
        }
        Ok(())
    }
}

/// Gives the device time of the timestamp `time`, or `None` when it is absent, 0, or
/// before the map of [`device`].
fn timestamp(time: Option<Stamp>) -> Option<Monotonic> {
    time.filter(|&time| time != Stamp::EPOCH).and_then(device)
}

/// The error of a change of the node of `read` with no timestamp.
#[cold]
fn no_timestamp(read: &Read) -> Error {
    Error::Config(vec![Diagnostic::new(
        NO_TIMESTAMP,
        None,
        format!(
            "the server gives a value of the node `{}` of the channel `{}` with no \
             timestamp, so the subscription cannot place it in time",
            read.node, read.channel,
        ),
        "Set `polled = true` to read each node at the rate".into(),
    )])
}
