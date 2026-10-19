//! The writes of a client: one write stream each, on the channels of one index.

use std::future::poll_fn;
use std::mem;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::task::Poll;

pub use program::Place;
pub use program::write::{Outcome, Refusal};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use types::authority::Authority;
use types::frame::{self, Form, Path};
use types::name::Name;
use types::sample::{Scalar, Type, Value};
use types::time::Span;

use crate::Error;
use program::write::Time;

/// What [`Client::open_writer`](crate::Client::open_writer) opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The channels that the writer writes, at least one, all on one index.
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

/// A writer of a [`Client`](crate::Client): one write stream on the channels of one
/// index, which the client's thread holds. It can move between threads. Each call
/// blocks until the node answers, and the node applies the frames in the order of the
/// calls. Dropping it stops the stream, and the node ends the writer;
/// [`Writer::close`] first waits until each frame is on disk.
///
/// Place `n` of each call is `Config::channels[n]`, and the index is place
/// `channels.len()` when `channels` does not name it.
#[derive(Debug)]
pub struct Writer {
    places: Vec<Place>,
    /// The place of the node of each place.
    nodes: Vec<usize>,
    index: usize,
    bytes_max: usize,
    calls: mpsc::Sender<Call>,
    written: Receiver<Written>,
    /// Set when the client closed or dropped.
    closed: Arc<AtomicBool>,
    /// The bytes of the next frame, in the places of the node, which go to the
    /// client's thread and back.
    frame: Frame,
}

impl Writer {
    /// Sends the open of `config` on `opens`, and blocks until the client's thread
    /// opened it. `closed` is set when the client closed or dropped.
    pub(crate) fn open(
        opens: &mpsc::UnboundedSender<Open>,
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
        opens.send(open).map_err(|_ended| lost(closed))?;
        let (node, index, bytes_max) = wait(&opened, closed)??;
        let (places, nodes, index) = ordered(&names, &node, index);
        Ok(Self {
            places,
            nodes,
            index,
            bytes_max,
            calls,
            written,
            closed: Arc::clone(closed),
            frame: Frame::default(),
        })
    }

    /// The channels of the writer: the key, sample type, and name of each place.
    /// Place `n` is `Config::channels[n]`, then the index when `Config::channels`
    /// does not name it.
    #[must_use]
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// The place of the index, whose samples are the stamps of each frame. When
    /// `Config::channels` names the index more than once, its first place.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// The most bytes of one frame of this writer, its head included: the largest
    /// block of the session's pool of frames. It does not change while the writer is
    /// open. [`Writer::write`] and [`Writer::write_series`] refuse a frame over it with
    /// `Pool(TooLarge)`, and series whose bytes alone go over it never fit.
    #[must_use]
    pub fn bytes_max(&self) -> usize {
        self.bytes_max
    }

    /// Writes a frame of one sample of each place of `values`, in any order, and
    /// blocks until the node took, lost, or refused it. A value of the index place is
    /// the stamp of the frame. With none, the node stamps it: mesh time now, or 1 ns
    /// after the last stamp that the node gave this writer when now is not after it.
    /// So after a stamp of the caller ahead of now, a frame that the node stamps can
    /// get [`Refusal::Order`].
    ///
    /// # Errors
    ///
    /// [`Error::Value`] when a value is not of the type of its place, with nothing
    /// sent; the writer goes on. [`Error::Closed`] when the client's session ended at
    /// a close or a drop, and [`Error::Panicked`] when the client's thread panicked.
    /// [`Error::Session`] when the session failed. Else [`Error::Stream`] with each
    /// error of `program::Writer::draft` and `program::Writer::write`: `Layout` for a
    /// place past the places (`OutOfRange`) or for two values of one channel
    /// (`Unordered`, `entry` the higher of their places and `last` the lower), each with
    /// nothing sent, and `Unsent` for no values.
    pub fn write(&mut self, values: &[(usize, Value)]) -> Result<Outcome, Error> {
        self.frame.clear(1);
        let index = self.nodes[self.index];
        let mut time = Time::Node;
        for &(place, value) in values {
            let (node, data_type) = self.place(place)?;
            let scalar = value.scalar();
            if data_type != Type::Scalar(scalar) {
                return Err(Error::Value {
                    place,
                    scalar,
                    data_type,
                });
            }
            if node == index {
                time = Time::Frame(Path::Live);
            }
            let part = Part { node, place };
            self.frame
                .push(part, scalar.width(), |sample| value.write(sample));
        }
        if time == Time::Node && !values.is_empty() {
            let part = Part {
                node: index,
                place: self.index,
            };
            self.frame.push(part, Scalar::Stamp.width(), |_| {});
        }
        self.call(time)
    }

    /// Writes a frame of `count` samples of each place of `series`, in any order, each
    /// in the raw form of its place's type, and blocks until the node took, lost, or
    /// refused it. The series of [`Writer::index`] holds the stamps of the frame. A
    /// frame of one channel of 8-byte samples and its index holds at most about 3,500
    /// samples (the largest block of the frames of the session, 57,344 bytes).
    ///
    /// # Errors
    ///
    /// [`Error::Closed`], [`Error::Panicked`], and [`Error::Session`] as
    /// [`Writer::write`] gives them. Else [`Error::Stream`] with each error of
    /// `program::Writer::draft` and `program::Writer::write`: `Layout` as
    /// [`Writer::write`] gives it, or `IndexAbsent` for series with none of the index,
    /// `entry` the lowest place of the series and `index` [`Writer::index`], each with
    /// nothing sent; `Unsent` for a frame with no series; and `Pool(TooLarge)` for a
    /// frame over the largest block, or, before any copy, for series whose bytes go
    /// over [`Writer::bytes_max`], `requested` their bytes.
    pub fn write_series(
        &mut self,
        count: u32,
        series: &[(usize, &[u8])],
    ) -> Result<Outcome, Error> {
        let requested = series.iter().map(|(_, bytes)| bytes.len()).sum();
        if requested > self.bytes_max {
            let largest = self.bytes_max;
            let large = block::Error::TooLarge { requested, largest };
            return Err(Error::Stream(program::Error::Pool(large)));
        }
        self.frame.clear(count);
        for &(place, bytes) in series {
            let (node, _) = self.place(place)?;
            self.frame.copy(Part { node, place }, bytes);
        }
        self.call(Time::Frame(Path::Live))
    }

    /// Finishes the stream, and blocks until each frame that the node took is on disk
    /// and the node finished the stream. Control ends then.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`], [`Error::Panicked`], [`Error::Session`], and
    /// [`Error::Stream`] as [`Writer::write`] gives them.
    pub fn close(self) -> Result<(), Error> {
        let (sender, finished) = sync_channel(1);
        self.send(Call::Close(sender))?;
        wait(&finished, &self.closed)?
    }

    /// The place of the node and the type of `place`.
    fn place(&self, place: usize) -> Result<(usize, Type), Error> {
        let past = || {
            let entries = self.places.len();
            let range = frame::Error::OutOfRange {
                entry: place,
                entries,
            };
            Error::Stream(program::Error::Layout(range))
        };
        let node = *self.nodes.get(place).ok_or_else(past)?;
        Ok((node, self.places[place].data_type))
    }

    /// Sends the frame with its stamps from `time`, and blocks for its outcome.
    fn call(&mut self, time: Time) -> Result<Outcome, Error> {
        self.sort()?;
        let frame = mem::take(&mut self.frame);
        self.send(Call::Write { frame, time })?;
        let (outcome, frame) = wait(&self.written, &self.closed)?;
        self.frame = frame;
        outcome
    }

    /// Puts the frame in the places of the node, and checks the rules of
    /// `frame::Layout` that the places of the caller break: no two series of one
    /// channel, and a series of the index in a frame of series. Each error names the
    /// places of the caller.
    fn sort(&mut self) -> Result<(), Error> {
        let layout = |error| Err(Error::Stream(program::Error::Layout(error)));
        self.frame.sort();
        let parts = self.frame.parts.iter().map(|&(part, _, _)| part);
        let mut last: Option<Part> = None;
        let mut indexed = false;
        for part in parts.clone() {
            if let Some(last) = last
                && last.node == part.node
            {
                let (entry, last) = (part.place, last.place);
                return layout(frame::Error::Unordered { entry, last });
            }
            indexed |= part.node == self.nodes[self.index];
            last = Some(part);
        }
        match parts.min_by_key(|part| part.place) {
            Some(part) if !indexed => layout(frame::Error::IndexAbsent {
                entry: part.place,
                index: self.index,
            }),
            _ => Ok(()),
        }
    }

    /// Sends `call` to the task of the writer.
    fn send(&self, call: Call) -> Result<(), Error> {
        self.calls.try_send(call).map_err(|error| match error {
            TrySendError::Closed(_) => lost(&self.closed),
            TrySendError::Full(_) => {
                unreachable!("invariant: each call takes its answer before the next")
            }
        })
    }
}

/// The places of the calls of a writer of `channels`, the place of the node of each,
/// and the place of the index, from the places of the node and the place of its index.
///
/// # Panics
///
/// When the node gives no place for a name of `channels`: `program` gives
/// `Unplaced` for such places.
fn ordered(
    channels: &[Name],
    node: &[Place],
    index: usize,
) -> (Vec<Place>, Vec<usize>, usize) {
    let of = |name: &Name| {
        let at = node.iter().position(|place| place.name == *name);
        at.expect("invariant: the node gives a place for each channel of the open")
    };
    let mut nodes: Vec<usize> = channels.iter().map(of).collect();
    let at = nodes.iter().position(|&n| n == index).unwrap_or_else(|| {
        nodes.push(index);
        nodes.len() - 1
    });
    let places = nodes.iter().map(|&n| node[n].clone()).collect();
    (places, nodes, at)
}

/// Blocks for the answer of the client's thread, or the error of [`lost`] when the
/// thread dropped its sender.
fn wait<T>(answers: &Receiver<T>, closed: &AtomicBool) -> Result<T, Error> {
    #[expect(
        clippy::disallowed_methods,
        reason = "a call of a writer blocks its caller, which may run a Tokio runtime"
    )]
    let answer = answers.recv();
    answer.map_err(|_ended| lost(closed))
}

/// The error of a call whose task ended: [`Error::Closed`] when the client closed or
/// dropped (`closed`), else [`Error::Panicked`], as only a panic ends the task first.
fn lost(closed: &AtomicBool) -> Error {
    if closed.load(Ordering::Acquire) {
        Error::Closed
    } else {
        Error::Panicked
    }
}

/// The places of the node, the place of its index, and the largest block of the
/// session's pool of frames.
type Opened = (Vec<Place>, usize, usize);

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

/// The outcome of a write, and its frame to fill again.
type Written = (Result<Outcome, Error>, Frame);

/// The bytes of a frame, kept between writes so that a warm write takes no
/// allocation.
#[derive(Debug, Default)]
struct Frame {
    count: u32,
    /// The places, start in `bytes`, and length of each series.
    parts: Vec<(Part, usize, usize)>,
    /// The place and length of each series, in place order, as `Layout` takes them.
    series: Vec<(usize, usize)>,
    bytes: Vec<u8>,
}

impl Frame {
    /// Empties the frame for a frame of `count` samples.
    fn clear(&mut self, count: u32) {
        self.count = count;
        self.parts.clear();
        self.bytes.clear();
    }

    /// Adds a series of `len` bytes at `part`, which `fill` writes.
    fn push(&mut self, part: Part, len: usize, fill: impl FnOnce(&mut [u8])) {
        let start = self.bytes.len();
        self.bytes.resize(start + len, 0);
        fill(&mut self.bytes[start..]);
        self.parts.push((part, start, len));
    }

    /// Adds `bytes` as a series at `part`.
    fn copy(&mut self, part: Part, bytes: &[u8]) {
        self.parts.push((part, self.bytes.len(), bytes.len()));
        self.bytes.extend_from_slice(bytes);
    }

    /// The place of the node and bytes of each series, in the order of the pushes, or
    /// in the order of the node's places after [`Frame::sort`].
    fn each(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let bytes = |&(part, start, len): &(Part, _, _)| {
            (part.node, &self.bytes[start..start + len])
        };
        self.parts.iter().map(bytes)
    }

    /// Puts the series in the order of the node's places, and series of one place of
    /// the node in the order of the caller's places.
    fn sort(&mut self) {
        self.parts
            .sort_unstable_by_key(|&(part, _, _)| (part.node, part.place));
        self.series.clear();
        let series = self.parts.iter().map(|&(part, _, len)| (part.node, len));
        self.series.extend(series);
    }
}

/// The places of a series of a [`Frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Part {
    /// The place of the node.
    node: usize,
    /// The place of the caller.
    place: usize,
}

/// Opens the writer of `open` on `session`, and serves the calls of its [`Writer`]
/// until the writer closes or drops, or `closed` changes. Its sender drops at the
/// close of the client, so the writer drops before the session closes. `bytes_max`
/// is the largest block of the session's pool of frames.
pub(crate) async fn serve(
    session: program::Session,
    bytes_max: usize,
    open: Open,
    mut closed: watch::Receiver<()>,
) {
    let mut served = pin!(served(session, bytes_max, open));
    let mut changed = pin!(closed.changed());
    poll_fn(|cx| {
        if changed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        served.as_mut().poll(cx)
    })
    .await;
}

async fn served(session: program::Session, bytes_max: usize, open: Open) {
    let Open {
        config,
        opened,
        mut calls,
        written,
    } = open;
    let failed = |error| match session.ended() {
        Some(cause) => Error::Session(cause),
        None => Error::Stream(error),
    };
    let mut writer = match session.writer(config).await {
        Ok(writer) => writer,
        Err(error) => {
            drop(opened.try_send(Err(failed(error))));
            return;
        }
    };
    let places = writer.places().to_vec();
    drop(opened.try_send(Ok((places, writer.index(), bytes_max))));
    while let Some(call) = calls.recv().await {
        match call {
            Call::Write { frame, time } => {
                let outcome = sent(&mut writer, &frame, time).await;
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

/// Drafts `frame` on `writer`, and writes it with its stamps from `time`.
async fn sent(
    writer: &mut program::Writer,
    frame: &Frame,
    time: Time,
) -> Result<Outcome, program::Error> {
    let draft = writer.draft(Form::Raw, frame.count, &frame.series).await?;
    // The draft holds the series of the frame, both in place order.
    for ((_, to), (_, bytes)) in draft.iter_mut().zip(frame.each()) {
        to.copy_from_slice(bytes);
    }
    writer.write(time).await
}

#[cfg(test)]
mod tests;
