//! The writes of a client: one write stream each, on channels of any index.

use std::mem;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

pub use program::write::{End, Outcome, Refusal};
use tokio::sync::mpsc;
use types::authority::Authority;
use types::frame::{self, Form, Path};
use types::name::Name;
use types::sample::{Scalar, Type, Value};
use types::time::Span;

use crate::{Error, Place, failed, lost, placed, send, wait};
use program::write::Time;

/// What [`Client::open_writer`](crate::Client::open_writer) opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The channels that the writer writes, at least one, of any index.
    pub channels: Vec<Name>,
    /// The writer's authority over its channels. [`Config::new`] gives 0, the least:
    /// a writer of a higher authority, such as a connector, takes control from it.
    pub authority: Authority,
    /// How long the writer may go with no write and keep control, or `None` for no
    /// limit, which [`Config::new`] gives. Control ends when the writer closes or
    /// drops.
    pub lease: Option<Span>,
}

impl Config {
    /// A writer of `channels` at authority 0 with no lease.
    #[must_use]
    pub fn new(channels: Vec<Name>) -> Self {
        Self {
            channels,
            authority: Authority(0),
            lease: None,
        }
    }
}

/// The samples of one place, for [`Writer::write_series`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Series<'a> {
    /// The place.
    pub place: usize,
    /// The count of the samples.
    pub count: u32,
    /// The samples, in the raw form of the place's type.
    pub bytes: &'a [u8],
}

/// A writer of a [`Client`](crate::Client): one write stream on channels of any index,
/// which the client's thread holds. The node writes the channels of each index as a
/// group, under its own control and lease. It can move between threads. Each call
/// blocks until the node answers, and the node applies the frames in the order of the
/// calls. Dropping it stops the stream, and the node ends the writer;
/// [`Writer::close`] first waits until each frame is on disk, or its group ended.
///
/// Place `n` of each call is `Config::channels[n]`. Then comes the index of each group
/// that `channels` does not name, in the order of the first channel of each group.
#[derive(Debug)]
pub struct Writer {
    places: Vec<Place>,
    /// The place of the node of each place.
    nodes: Vec<usize>,
    /// The first place of each place of the node.
    firsts: Vec<usize>,
    bytes_max: usize,
    calls: mpsc::Sender<Call>,
    written: Receiver<Written>,
    /// Set when the client closed or dropped.
    closed: Arc<AtomicBool>,
    /// The error of a call whose answer did not come. The task ended, and the call
    /// can still fill `calls`.
    lost: Option<Error>,
    /// The bytes of the next frame, in the places of the node, which go to the
    /// client's thread and back.
    frame: Frame,
    /// The lowest place and count of each group by the place of the node of its
    /// index, for the check of the counts of a frame of series.
    counts: Vec<Option<(usize, u32)>>,
}

impl Writer {
    /// Sends the open of `config` on `opens`, and blocks until the client's thread
    /// opened it. `closed` is set when the client closed or dropped.
    pub(crate) fn open(
        opens: &mpsc::UnboundedSender<crate::Open>,
        closed: &Arc<AtomicBool>,
        config: Config,
    ) -> Result<Self, Error> {
        let (calls, received) = mpsc::channel(1);
        let (answer, written) = sync_channel(1);
        let (sender, opened) = sync_channel(1);
        let names = config.channels.clone();
        let open = Open {
            config: program::write::Config {
                authority: config.authority,
                lease: config.lease,
                channels: config.channels,
            },
            opened: sender,
            calls: received,
            written: answer,
        };
        let open = crate::Open::Write(open);
        opens.send(open).map_err(|_ended| lost(closed))?;
        let (node, bytes_max) = wait(&opened, closed)??;
        // `program` gives `Unplaced` for an open whose places miss a name.
        let (nodes, places) = placed(&names, &node)
            .expect("invariant: the node gives a place for each channel of the open");
        let mut firsts = vec![usize::MAX; node.len()];
        for (place, &node) in nodes.iter().enumerate().rev() {
            firsts[node] = place;
        }
        Ok(Self {
            places,
            nodes,
            firsts,
            bytes_max,
            calls,
            written,
            closed: Arc::clone(closed),
            lost: None,
            frame: Frame::default(),
            counts: vec![None; node.len()],
        })
    }

    /// The channels of the writer: the key, sample type, name, and place of the index
    /// of each place. Place `n` is `Config::channels[n]`, then the index of each group
    /// that `Config::channels` does not name, in the order of the first channel of each
    /// group. Each place names the first place of its index as its index, a later place
    /// of the index too.
    #[must_use]
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// The most bytes of one frame of this writer, its head included: the largest
    /// block of the session's pool of frames. It does not change while the writer is
    /// open. [`Writer::write`] and [`Writer::write_series`] refuse a frame over it with
    /// `Pool(TooLarge)`, and series whose bytes alone go over it never fit.
    #[must_use]
    pub fn bytes_max(&self) -> usize {
        self.bytes_max
    }

    /// Writes a frame of one sample of each place of `values`, in any order, and blocks
    /// until the node answered each group of it. Gives the place of the index and the
    /// outcome of each present group, in the order of their index places: `Taken`,
    /// `Lost`, or `Refused`. The slice borrows the writer, so the next call ends it. A
    /// value at the index of a group is its stamp. The values give the stamp of each
    /// present group, or of none: then the node stamps each group, at mesh time now, or
    /// 1 ns after the last stamp that it gave that group of the writer when now is not
    /// after it. So after a stamp of the caller ahead of now, a frame that the node
    /// stamps can get [`Refusal::Order`].
    ///
    /// # Errors
    ///
    /// [`Error::Value`] when a value is not of the type of its place, with nothing
    /// sent; the writer goes on. [`Error::Partial`] when the write of a group of the
    /// frame ended, and each later frame of the group gets it again; the writer goes
    /// on. [`Error::Closed`] when the client's session ended at a close or a drop, and
    /// [`Error::Panicked`] when the client's thread panicked. [`Error::Session`] when
    /// the session failed. Else [`Error::Stream`] with each error of
    /// `program::Writer::draft` and `program::Writer::write`, in places of the caller:
    /// `Layout` for a place past the places (`OutOfRange`), for two values of one
    /// channel (`Unordered`, `entry` the higher of their places and `last` the lower),
    /// or for values that give the stamps of only some groups (`IndexAbsent`, `entry`
    /// the lowest place of a group with no stamp and `index` the place of its index),
    /// each with nothing sent, and `Unsent` for no values.
    pub fn write(
        &mut self,
        values: &[(usize, Value)],
    ) -> Result<&[(usize, Outcome)], Error> {
        self.frame.parts.clear();
        self.frame.bytes.clear();
        let mut stamped = false;
        for &(place, value) in self.bounded(values) {
            let (node, group, data_type) = self.place(place)?;
            let scalar = value.scalar();
            if data_type != Type::Scalar(scalar) {
                return Err(Error::Value {
                    place,
                    scalar,
                    data_type,
                });
            }
            stamped |= node == group;
            let fill = |sample: &mut [u8]| value.write(sample);
            self.frame.push(node, group, place, scalar.width(), fill);
        }
        if stamped {
            return self.call(Time::Frame(Path::Live));
        }
        self.stamps();
        self.call(Time::Node)
    }

    /// Writes a frame of each series of `series`, in any order, and blocks as
    /// [`Writer::write`] does. Each present group holds the series of its index, the
    /// stamps of its samples, and each series of one group holds one count. A frame of
    /// one channel of 8-byte samples and its index holds at most about 3,500 samples
    /// (the largest block of the frames of the session, 57,344 bytes).
    ///
    /// # Errors
    ///
    /// [`Error::Partial`], [`Error::Closed`], [`Error::Panicked`], and
    /// [`Error::Session`] as [`Writer::write`] gives them. Else [`Error::Stream`] with
    /// each error of `program::Writer::draft` and `program::Writer::write`, in places
    /// of the caller: `Layout` as [`Writer::write`] gives it, or `IndexAbsent` for a
    /// group with no series of its index, `entry` the lowest place of such a group and
    /// `index` the place of its index; `Count` for two series of one group with two
    /// counts, `place` the lowest place of the group and `other` the lowest place of it
    /// whose count is not that count; each with nothing sent. `Unsent` for a frame
    /// with no series or a series of no samples; and `Pool(TooLarge)` for a frame over
    /// the largest block, or, before any copy, for series whose bytes go over
    /// [`Writer::bytes_max`], `requested` their bytes.
    pub fn write_series(
        &mut self,
        series: &[Series<'_>],
    ) -> Result<&[(usize, Outcome)], Error> {
        let requested = series.iter().map(|series| series.bytes.len()).sum();
        if requested > self.bytes_max {
            let largest = self.bytes_max;
            let large = block::Error::TooLarge { requested, largest };
            return Err(Error::Stream(program::Error::Pool(large)));
        }
        self.frame.parts.clear();
        self.frame.bytes.clear();
        for &Series {
            place,
            count,
            bytes,
        } in self.bounded(series)
        {
            let (node, group, _) = self.place(place)?;
            self.frame.copy(node, group, place, count, bytes);
        }
        self.call(Time::Frame(Path::Live))
    }

    /// Finishes the stream, and blocks until each frame that the node took is on disk,
    /// or its group ended, and the node finished the stream. Control ends then. The end
    /// of a group alone gives no error at the close.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`], [`Error::Panicked`], [`Error::Session`], and
    /// [`Error::Stream`] as [`Writer::write`] gives them. [`Error::Stream`] with
    /// [`program::Error::Unstored`] for each group that ended with a frame that the
    /// node took and that may not be on disk, with the place of the index of each such
    /// group.
    pub fn close(self) -> Result<(), Error> {
        if let Some(error) = self.lost {
            return Err(error);
        }
        let (sender, finished) = sync_channel(1);
        send(&self.calls, Call::Close(sender), &self.closed)?;
        wait(&finished, &self.closed)?.map_err(|error| match error {
            Error::Stream(program::Error::Unstored(mut groups)) => {
                for (index, _) in &mut groups {
                    *index = self.firsts[*index];
                }
                groups.sort_unstable_by_key(|&(index, _)| index);
                Error::Stream(program::Error::Unstored(groups))
            }
            error => error,
        })
    }

    /// The first items of a call, at most one more than the places. A frame holds no
    /// place twice, so each item past the places repeats one of these, and the sort
    /// gives `Unordered` with the kept frame bounded by the places.
    fn bounded<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        &items[..items.len().min(self.places.len() + 1)]
    }

    /// The place of the node of `place`, the place of the node of its group's index,
    /// and its type.
    fn place(&self, place: usize) -> Result<(usize, usize, Type), Error> {
        let past = || {
            let entries = self.places.len();
            let range = frame::Error::OutOfRange {
                entry: place,
                entries,
            };
            Error::Stream(program::Error::Layout(range))
        };
        let node = *self.nodes.get(place).ok_or_else(past)?;
        let Place {
            index, data_type, ..
        } = self.places[place];
        Ok((node, self.nodes[index], data_type))
    }

    /// Adds an empty stamp at the index of each group of the frame, which holds no
    /// series of an index.
    fn stamps(&mut self) {
        let frame = &mut self.frame;
        frame.parts.sort_unstable_by_key(|part| part.group);
        for at in 0..frame.parts.len() {
            let group = frame.parts[at].group;
            if at == 0 || group != frame.parts[at - 1].group {
                let width = Scalar::Stamp.width();
                frame.push(group, group, self.firsts[group], width, |_| {});
            }
        }
    }

    /// Sends the frame with its stamps from `time`, and blocks for the outcome of each
    /// group, in places of the caller.
    fn call(&mut self, time: Time) -> Result<&[(usize, Outcome)], Error> {
        if let Some(error) = &self.lost {
            return Err(error.clone());
        }
        self.sort()?;
        let frame = mem::take(&mut self.frame);
        send(&self.calls, Call::Write { frame, time }, &self.closed)?;
        let (written, frame) = wait(&self.written, &self.closed)
            .map_err(|error| self.lost.insert(error).clone())?;
        self.frame = frame;
        written?;
        for (index, _) in &mut self.frame.outcomes {
            *index = self.firsts[*index];
        }
        self.frame
            .outcomes
            .sort_unstable_by_key(|&(index, _)| index);
        let ended =
            |(_, outcome): &(usize, Outcome)| matches!(outcome, Outcome::Ended(_));
        if self.frame.outcomes.iter().any(ended) {
            return Err(self.partial());
        }
        Ok(&self.frame.outcomes)
    }

    /// [`Error::Partial`] of the frame and its outcomes.
    fn partial(&self) -> Error {
        let outcomes = &self.frame.outcomes;
        let mut places: Vec<_> =
            self.frame.parts.iter().map(|part| part.place).collect();
        places.sort_unstable();
        let (mut written, mut unwritten) = (Vec::new(), Vec::new());
        for place in places {
            let index = self.places[place].index;
            let at = outcomes
                .binary_search_by_key(&index, |&(index, _)| index)
                .expect("invariant: each group of the frame has its outcome");
            let name = self.places[place].name.clone();
            match outcomes[at].1 {
                Outcome::Taken { .. } => written.push(name),
                _ => unwritten.push(name),
            }
        }
        Error::Partial {
            written,
            unwritten,
            outcomes: outcomes.clone(),
        }
    }

    /// Puts the frame in the places of the node, and checks the rules of
    /// `frame::Layout` and of the counts that the places of the caller break: no two
    /// series of one channel, a series of the index of each group, and one count in
    /// each group. Each error names the places of the caller.
    fn sort(&mut self) -> Result<(), Error> {
        let layout = |error| Err(Error::Stream(program::Error::Layout(error)));
        self.frame.sort();
        let parts = &self.frame.parts;
        for pair in parts.windows(2) {
            if let [last, next] = pair
                && last.node == next.node
            {
                let (entry, last) = (next.place, last.place);
                return layout(frame::Error::Unordered { entry, last });
            }
        }
        let absent = parts
            .iter()
            .filter(|part| {
                (parts.binary_search_by_key(&part.group, |part| part.node)).is_err()
            })
            .map(|part| part.place)
            .min();
        if let Some(entry) = absent {
            let index = self.places[entry].index;
            return layout(frame::Error::IndexAbsent { entry, index });
        }
        self.counted()
    }

    /// Checks that each group of the frame holds one count.
    fn counted(&mut self) -> Result<(), Error> {
        let parts = &self.frame.parts;
        let first = parts.first().map_or(0, |part| part.count);
        if parts.iter().all(|part| part.count == first) {
            return Ok(());
        }
        let counts = &mut self.counts;
        counts.fill(None);
        for part in parts {
            let lowest = &mut counts[part.group];
            let part = (part.place, part.count);
            *lowest = Some(lowest.map_or(part, |lowest| lowest.min(part)));
        }
        let other = |part: &Part| {
            let (place, count) = counts[part.group]?;
            (part.count != count).then_some((place, count, part.place, part.count))
        };
        match parts.iter().filter_map(other).min() {
            Some((place, count, other, other_count)) => {
                Err(Error::Stream(program::Error::Count {
                    place,
                    count,
                    other,
                    other_count,
                }))
            }
            None => Ok(()),
        }
    }
}

/// The places of the node, and the largest block of the session's pool of frames.
type Opened = (Vec<program::Place>, usize);

/// The open of a writer, which the client's thread serves in a task of its own.
#[derive(Debug)]
pub(crate) struct Open {
    config: program::write::Config,
    opened: SyncSender<Result<Opened, Error>>,
    calls: mpsc::Receiver<Call>,
    written: SyncSender<Written>,
}

/// A call of a [`Writer`] to its task.
#[derive(Debug)]
enum Call {
    Write { frame: Frame, time: Time },
    Close(SyncSender<Result<(), Error>>),
}

/// The result of a write, its outcomes in its frame, and its frame to fill again.
type Written = (Result<(), Error>, Frame);

/// The bytes of a frame and the outcomes of its groups, kept between writes so that a
/// warm write takes no allocation.
#[derive(Debug, Default)]
struct Frame {
    /// Each series.
    parts: Vec<Part>,
    /// The place, count, and length of each series, in place order, as
    /// `Writer::draft` takes them.
    series: Vec<(usize, u32, usize)>,
    bytes: Vec<u8>,
    /// The place of the index and the outcome of each group, as
    /// `program::Writer::write` gives them, then in places of the caller.
    outcomes: Vec<(usize, Outcome)>,
}

impl Frame {
    /// Adds a series of one sample of `len` bytes at `node` and `place`, in the group
    /// of `group`, which `fill` writes.
    fn push(
        &mut self,
        node: usize,
        group: usize,
        place: usize,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) {
        let start = self.bytes.len();
        self.bytes.resize(start + len, 0);
        fill(&mut self.bytes[start..]);
        self.parts.push(Part {
            node,
            group,
            place,
            count: 1,
            start,
            len,
        });
    }

    /// Adds `bytes`, a series of `count` samples, at `node` and `place`, in the group
    /// of `group`.
    fn copy(
        &mut self,
        node: usize,
        group: usize,
        place: usize,
        count: u32,
        bytes: &[u8],
    ) {
        self.parts.push(Part {
            node,
            group,
            place,
            count,
            start: self.bytes.len(),
            len: bytes.len(),
        });
        self.bytes.extend_from_slice(bytes);
    }

    /// The place of the node and bytes of each series, in the order of the pushes, or
    /// in the order of the node's places after [`Frame::sort`].
    fn each(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let bytes =
            |part: &Part| (part.node, &self.bytes[part.start..part.start + part.len]);
        self.parts.iter().map(bytes)
    }

    /// Puts the series in the order of the node's places, and series of one place of
    /// the node in the order of the caller's places.
    fn sort(&mut self) {
        self.parts
            .sort_unstable_by_key(|part| (part.node, part.place));
        self.series.clear();
        let series = (self.parts.iter()).map(|part| (part.node, part.count, part.len));
        self.series.extend(series);
    }
}

/// A series of a [`Frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Part {
    /// The place of the node.
    node: usize,
    /// The place of the node of the index of its group.
    group: usize,
    /// The place of the caller.
    place: usize,
    count: u32,
    /// The start of its bytes in `Frame::bytes`.
    start: usize,
    len: usize,
}

/// Opens the writer of `open` on `session`, and serves the calls of its [`Writer`]
/// until the writer closes or drops. `bytes_max` is the largest block of the
/// session's pool of frames.
pub(crate) async fn served(session: program::Session, bytes_max: usize, open: Open) {
    let Open {
        config,
        opened,
        mut calls,
        written,
    } = open;
    let failed = |error| failed(&session, error);
    let mut writer = match session.writer(config).await {
        Ok(writer) => writer,
        Err(error) => {
            drop(opened.try_send(Err(failed(error))));
            return;
        }
    };
    let places = writer.places().to_vec();
    drop(opened.try_send(Ok((places, bytes_max))));
    while let Some(call) = calls.recv().await {
        match call {
            Call::Write { mut frame, time } => {
                let outcome = sent(&mut writer, &mut frame, time).await;
                drop(written.try_send((outcome.map_err(failed), frame)));
            }
            Call::Close(finished) => {
                let closed = writer.finish().await.map_err(failed);
                drop(finished.try_send(closed));
                return;
            }
        }
    }
}

/// Drafts `frame` on `writer`, writes it with its stamps from `time`, and puts the
/// outcomes in `frame`.
async fn sent(
    writer: &mut program::Writer,
    frame: &mut Frame,
    time: Time,
) -> Result<(), program::Error> {
    let mut draft = writer.draft(Form::Raw, &frame.series).await?;
    // The draft holds the series of the frame, both in place order.
    for ((_, to), (_, bytes)) in draft.iter_mut().zip(frame.each()) {
        to.copy_from_slice(bytes);
    }
    let outcomes = writer.write(time).await?;
    frame.outcomes.clear();
    frame.outcomes.extend_from_slice(outcomes);
    Ok(())
}

#[cfg(test)]
mod tests;
