//! The reads of a client: one read stream of the channels of one index.

use std::future::poll_fn;
use std::mem;
use std::ops::Range;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::task::Poll;
use std::time::Duration;

pub use program::Mode;
use program::Refusal;
use tokio::sync::mpsc;
use types::name::{Name, Selector, Written};

use crate::{Error, Place, failed, lost, placed, send, wait};

/// What [`Client::open_reader`](crate::Client::open_reader) opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The channels that the reader reads, at least one, all of one index.
    pub channels: Vec<Name>,
    /// Which frames the reader gets. [`Config::new`] gives [`Mode::Complete`]: each
    /// frame from the open on, in order, past a loss of the node's buffer with the
    /// lost seqs. [`Mode::Latest`] skips by design each frame that a newer one replaces
    /// before the node sends it, with no lost seqs. The frames that the client got
    /// come in order.
    pub mode: Mode,
}

impl Config {
    /// A complete reader of `channels`.
    #[must_use]
    pub fn new(channels: Vec<Name>) -> Self {
        Self {
            channels,
            mode: Mode::Complete,
        }
    }
}

/// A reader of a [`Client`](crate::Client): one read stream, which the client's thread
/// holds. It can move between threads. Its frames come in the order of their stamps.
/// Dropping it stops the stream, and the node ends the read.
///
/// Each frame that the node sent and the caller did not read holds a part of the
/// receive window of the client's session. When the window is full, each stream of the
/// client waits: the answers to its writers, its other readers, and the renewal of its
/// hello, and the node closes the session when the hello ends. Read each reader as its
/// frames come, or read on a client of its own.
#[derive(Debug)]
pub struct Reader {
    places: Vec<Place>,
    calls: mpsc::Sender<Frame>,
    answers: Receiver<Answer>,
    /// Set when the client closed or dropped.
    closed: Arc<AtomicBool>,
    /// The frame of the last call, which goes to the client's thread and back.
    frame: Frame,
    /// Set while a call that [`Reader::next_within`] sent has no answer yet.
    pending: bool,
    /// The error that ended the reader.
    ended: Option<Error>,
}

impl Reader {
    /// Sends the open of `config` on `opens`, and blocks until the client's thread
    /// opened it. `closed` is set when the client closed or dropped.
    pub(crate) fn open(
        opens: &mpsc::UnboundedSender<crate::Open>,
        closed: &Arc<AtomicBool>,
        config: Config,
    ) -> Result<Self, Error> {
        if config.channels.is_empty() {
            return Err(Error::Stream(program::Error::Unnamed));
        }
        let (calls, received) = mpsc::channel(1);
        let (answer, answers) = sync_channel(1);
        let (sender, opened) = sync_channel(1);
        let open = Open {
            config,
            opened: sender,
            calls: received,
            answer,
        };
        let open = crate::Open::Read(open);
        opens.send(open).map_err(|_ended| lost(closed))?;
        Ok(Self {
            places: wait(&opened, closed)??,
            calls,
            answers,
            closed: Arc::clone(closed),
            frame: Frame::new(),
            pending: false,
            ended: None,
        })
    }

    /// The channels of the reader: the key, sample type, and name of each place.
    /// Place `n` is `Config::channels[n]`, so a channel named twice is at each of its
    /// places. Then the index, when `Config::channels` does not name it.
    #[must_use]
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// Blocks until the next frame. The frame is the reader's own, and the next call
    /// fills it again.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the client's session ended at a close or a drop, and
    /// [`Error::Panicked`] when the client's thread panicked. [`Error::Session`] when
    /// the session failed. [`Error::Decode`] when a series does not decode. Else
    /// [`Error::Stream`] with each error of `program::Reader::next`, such as
    /// `Refused(Refusal::Unknown)` when a channel was removed. After an error, each
    /// call gives the same error.
    #[expect(
        clippy::should_implement_trait,
        reason = "the frame borrows the reader, which `Iterator` cannot give"
    )]
    pub fn next(&mut self) -> Result<&Frame, Error> {
        self.sent()?;
        let answer = wait(&self.answers, &self.closed);
        self.answered(answer)
    }

    /// Like [`Reader::next`], but gives `None` when no frame came within `timeout`.
    /// The call stays out: the next call of `next` or `next_within` waits for its
    /// frame, so no frame is lost. The wait reads the monotonic clock of the OS.
    ///
    /// # Errors
    ///
    /// The errors of [`Reader::next`].
    pub fn next_within(&mut self, timeout: Duration) -> Result<Option<&Frame>, Error> {
        self.sent()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "a call of a stream blocks its caller, and the bound of the wait is \
                      no time of data"
        )]
        let answer = match self.answers.recv_timeout(timeout) {
            Ok(answer) => Ok(answer),
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(lost(&self.closed)),
        };
        self.answered(answer).map(Some)
    }

    /// Sends the frame of the last call to the client's thread, unless a call is out.
    /// Gives the error that ended the reader.
    fn sent(&mut self) -> Result<(), Error> {
        if let Some(error) = &self.ended {
            return Err(error.clone());
        }
        if !self.pending {
            let frame = mem::replace(&mut self.frame, Frame::new());
            send(&self.calls, frame, &self.closed)
                .map_err(|error| self.ended.insert(error).clone())?;
            self.pending = true;
        }
        Ok(())
    }

    /// The frame of `answer`, the answer to the call that is out, or the error that
    /// ends the reader.
    fn answered(&mut self, answer: Result<Answer, Error>) -> Result<&Frame, Error> {
        // A lost answer ends the reader too: its call can still fill `calls`.
        let answer = answer.and_then(|(answer, frame)| {
            self.pending = false;
            self.frame = frame;
            answer
        });
        match answer {
            Ok(()) => Ok(&self.frame),
            Err(error) => Err(self.ended.insert(error).clone()),
        }
    }
}

/// A frame of a [`Reader`]: the series of the channels of one index, `count` samples
/// each, at the stamps of the series of the index.
#[derive(Debug)]
pub struct Frame {
    count: u32,
    index: usize,
    /// The series of each place, held only at the first place of each entry. Those
    /// that the frame does not hold keep their bytes for later frames.
    series: Vec<Series>,
    /// The place of the series of each place: the first place of its entry.
    firsts: Vec<usize>,
}

/// The series of a place of a [`Frame`].
#[derive(Debug, Default)]
struct Series {
    /// Set when the frame holds the series.
    held: bool,
    /// The samples in the raw form of the type of the place.
    bytes: Vec<u8>,
    lost: Range<u64>,
}

impl Frame {
    /// A frame of no series, which allocates nothing.
    const fn new() -> Self {
        Self {
            count: 0,
            index: 0,
            series: Vec::new(),
            firsts: Vec::new(),
        }
    }

    /// The samples of each series.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The place of the index of the frame. Its series holds the stamps of the frame.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// The series of `place`, in the raw form of its type, or `None` when the frame
    /// holds no series of it.
    #[must_use]
    pub fn series(&self, place: usize) -> Option<&[u8]> {
        self.find(place).map(|series| series.bytes.as_slice())
    }

    /// The seqs of the index of `place` lost after the last series of `place` with a
    /// sample, up to this frame: one range from the first lost seq to the last. A seq
    /// in it that the reader got held no sample of `place`. Empty when the frame holds
    /// no sample of `place`, when none was lost, and for a latest reader.
    #[must_use]
    pub fn lost(&self, place: usize) -> Range<u64> {
        self.find(place).map_or(0..0, |series| series.lost.clone())
    }

    fn find(&self, place: usize) -> Option<&Series> {
        let first = *self.firsts.get(place)?;
        self.series.get(first).filter(|series| series.held)
    }
}

/// The open of a reader, which the client's thread serves in a task of its own.
#[derive(Debug)]
pub(crate) struct Open {
    config: Config,
    opened: SyncSender<Result<Vec<Place>, Error>>,
    calls: mpsc::Receiver<Frame>,
    answer: SyncSender<Answer>,
}

/// The outcome of a call of [`Reader::next`], and its frame.
type Answer = (Result<(), Error>, Frame);

/// Opens the reader of `open` on `session`, and serves the calls of its [`Reader`]
/// until the reader ends or drops, also while a call is out.
pub(crate) async fn served(session: program::Session, open: Open) {
    let Open {
        config,
        opened,
        mut calls,
        answer,
    } = open;
    let (mut reader, layout) = match reader(&session, &config).await {
        Ok(opened) => opened,
        Err(error) => {
            drop(opened.try_send(Err(failed(&session, error))));
            return;
        }
    };
    drop(opened.try_send(Ok(layout.places)));
    while let Some(mut frame) = calls.recv().await {
        let read = {
            let mut next = pin!(reader.next());
            poll_fn(|cx| match calls.poll_recv(cx) {
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Ready(Some(_)) => {
                    unreachable!(
                        "invariant: each call takes its answer before the next"
                    )
                }
                Poll::Pending => next.as_mut().poll(cx).map(Some),
            })
            .await
        };
        let Some(read) = read else {
            return;
        };
        let next = match read {
            Ok(encoded) => {
                let lost = |entry| reader.lost(entry);
                decoded(reader.places(), &layout.targets, lost, &encoded, &mut frame)
            }
            Err(error) => Err(failed(&session, error)),
        };
        let ended = next.is_err();
        drop(answer.try_send((next, frame)));
        if ended {
            return;
        }
    }
}

/// Opens a reader of `config.channels` on `session`, and gives its layout. The node
/// opens a reader of the names that match a channel, so `sdk`, not the node, gives
/// `Refused(Refusal::Unknown)` for a name of no channel among others.
async fn reader(
    session: &program::Session,
    config: &Config,
) -> Result<(program::Reader, Layout), program::Error> {
    let names = &config.channels;
    let written = names.iter().map(|name| Written::Include(name.as_str()));
    let select = Selector::from_written(written)
        .expect("invariant: a name reads as a pattern, and the names are not empty");
    let reader = session.reader(select, config.mode).await?;
    let layout = layout(names, reader.places(), reader.index())
        .ok_or(program::Error::Refused(Refusal::Unknown))?;
    Ok((reader, layout))
}

/// The places of a reader, and where the series of its frames go in them.
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    places: Vec<Place>,
    targets: Targets,
}

/// Where the series of the frames of a `program::Reader` go in the places of its
/// reader.
#[derive(Debug, PartialEq, Eq)]
struct Targets {
    /// The place of the index.
    index: usize,
    /// The first place of each entry, in order, or `None` for an entry of no place.
    entries: Vec<Option<usize>>,
    /// The first place of the entry of each place of the reader.
    firsts: Vec<usize>,
}

/// The layout of a reader of `names` from `entries`, the places of a
/// `program::Reader`, whose index is entry `index`: place `n` is `names[n]`, then the
/// index when `names` does not name it. `None` when a name is no entry.
fn layout(names: &[Name], entries: &[Place], index: usize) -> Option<Layout> {
    let (each, places, index) = placed(names, entries, index)?;
    let mut targets = vec![None; entries.len()];
    let firsts = (each.iter().enumerate())
        .map(|(place, &entry)| *targets[entry].get_or_insert(place))
        .collect();
    let targets = Targets {
        index,
        entries: targets,
        firsts,
    };
    Some(Layout { places, targets })
}

/// The samples of each series of `encoded`, a frame of a reader, as a `u32` and a
/// `usize`.
fn count(encoded: &types::frame::Frame) -> (u32, usize) {
    let count = encoded
        .range(0)
        .expect("invariant: a frame of a reader holds the index group")
        .count;
    let samples = usize::try_from(count).expect("invariant: a u32 fits a usize");
    (count, samples)
}

/// Decodes `encoded`, a frame of a reader of `places`, into `frame` at the places of
/// `targets`, with the `lost` seqs of each entry with a series.
fn decoded(
    places: &[Place],
    targets: &Targets,
    lost: impl Fn(usize) -> Range<u64>,
    encoded: &types::frame::Frame,
    frame: &mut Frame,
) -> Result<(), Error> {
    let (count, samples) = count(encoded);
    frame.count = count;
    frame.index = targets.index;
    // A frame serves one reader, so its layout is set at its first frame only.
    if frame.firsts.len() != targets.firsts.len() {
        frame
            .series
            .resize_with(targets.firsts.len(), Series::default);
        frame.firsts.clone_from(&targets.firsts);
    }
    for series in &mut frame.series {
        series.held = false;
    }
    for (entry, bytes) in encoded.iter() {
        let Some(first) = targets.entries[entry] else {
            continue;
        };
        let series = &mut frame.series[first];
        let data_type = places[entry].data_type;
        codec::decode(data_type, samples, bytes, &mut series.bytes).map_err(
            |cause| Error::Decode {
                place: first,
                cause,
            },
        )?;
        series.lost = lost(entry);
        series.held = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
