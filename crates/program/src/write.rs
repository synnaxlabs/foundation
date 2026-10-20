//! The writes of a session: one write stream each, on the channels of one index.

use std::iter;
use std::rc::Rc;
use std::sync::Arc;

use transport::stream::{Part, Receiver, Sender};
use transport::{Class, Code};
use types::authority::Authority;
use types::frame::key_set::KeySet;
use types::frame::{Draft, Form, Layout, Path};
use types::name::Name;
use types::time::Span;
use wire::hub::client::Place;
use wire::hub::client::write::{Open, Opening, Start, Write};
pub use wire::hub::write::{End, Outcome, Refusal, Time};
use wire::hub::write::{Lead, Replies, Reply};

use crate::{Error, Handle, end};

/// What a [`Writer`] opens with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The writer's authority over its channels.
    pub authority: Authority,
    /// How long the writer may go without a write and keep control, or `None` for no
    /// limit.
    pub lease: Option<Span>,
    /// The channels that the writer writes, at least one, all on one index.
    pub channels: Vec<Name>,
}

/// A writer of a [`Session`](crate::Session): one write stream on the channels of one
/// index, at its home. The home applies its frames in the order of the calls. It
/// holds a clone of its session. [`Writer::finish`] ends it once its frames are on
/// disk. Dropping it stops the stream, and the home ends the writer.
#[derive(Debug)]
pub struct Writer {
    session: Rc<Handle>,
    /// `None` once the writer ended.
    stream: Option<(Sender, Receiver)>,
    places: Vec<Place>,
    set: Arc<KeySet>,
    /// The frame that the next write sends.
    draft: Option<Draft>,
    replies: Replies,
    /// A call dropped while it sent a frame, so the stream holds part of it.
    cut: bool,
    /// The error that ended the writer.
    ended: Option<Error>,
}

impl Writer {
    /// Opens the write of `open` on `session`, whose length is at most
    /// `OPEN_BYTES_MAX`. Gives each error of
    /// [`Session::writer`](crate::Session::writer) but [`Error::Unnamed`] and
    /// [`Error::Open`], with the error of the close in place of the error that ended
    /// the renewal. Dropping the stream on `Pool` stops it.
    pub(crate) async fn open(session: Rc<Handle>, open: &Open) -> Result<Self, Error> {
        let (mut sender, mut receiver) =
            session.0.session.open(Class::Complete).await?;
        let Start { places, set } =
            match start(&session, &mut sender, &mut receiver, open).await {
                Ok(started) => started,
                Err(error) => {
                    end((sender, receiver), &error);
                    return Err(error);
                }
            };
        let lead = Lead::most(&set);
        let largest = session.0.pool.largest();
        if lead > largest {
            return Err(Error::Pool(block::Error::TooLarge {
                requested: lead,
                largest,
            }));
        }
        Ok(Self {
            session,
            stream: Some((sender, receiver)),
            places,
            replies: Replies::new(set.groups().len()),
            set,
            draft: None,
            cut: false,
            ended: None,
        })
    }

    /// The channels of the writer: the key, sample type, and name of each place.
    /// Place `n` is entry `n` of each draft.
    #[must_use]
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// The place of the index of the writer's channels, whose samples are the stamps
    /// of each frame. A frame of at least one sample holds its series.
    #[must_use]
    pub fn index(&self) -> usize {
        self.set.groups()[0]
    }

    /// The draft that the next [`Writer::write`] sends, to fill: a new frame of the
    /// writer's key set from [`Config::frames`](crate::Config::frames), with `count`
    /// samples in each series, as [`Draft::new`] takes `series`. The entry of place `n`
    /// is `n`. A frame of at least one sample holds the index series. It drops a held
    /// draft that no write sent. While the pool is full, it waits as each block of the
    /// session waits: the blocks of a frame come back as the node acknowledges it. The
    /// writer holds the draft, so a draft of another writer reaches its write only
    /// through a swap of two `&mut Draft`.
    ///
    /// # Errors
    ///
    /// [`Error::Layout`] when `series` breaks a rule of [`Layout::new`], with no block
    /// taken; the writer goes on. [`Error::Pool`] when no block of the pool can hold
    /// the frame, the error of the close when the session closes first, and the error
    /// that ended the writer, as [`Writer::write`] gives it. The writer then holds no
    /// draft.
    pub async fn draft(
        &mut self,
        form: Form,
        count: u32,
        series: &[(usize, usize)],
    ) -> Result<&mut Draft, Error> {
        self.draft = None;
        self.check()?;
        let shared = &self.session.0;
        let layout = Layout::new(&self.set, series).map_err(Error::Layout)?;
        let mut draft = shared.retry(|| layout.draft(&shared.frames, form)).await?;
        draft.set_counts(0, iter::repeat(count));
        Ok(self.draft.insert(draft))
    }

    /// Sends the writer's draft with its stamps from `time`, and waits for what the
    /// home did with it. A frame of [`Time::Node`] holds one sample of each series,
    /// in the raw form, and the home writes a stamp over its index sample: mesh time
    /// now, or 1 ns after the last stamp of this writer when now is not after it.
    /// Behind a later stamp of another writer, the home refuses it with
    /// [`Refusal::Order`]. A refused or lost frame leaves the writer open. A removed
    /// channel or a failed commit of a group gives [`Outcome::Ended`], also for each
    /// later frame of the group. A call dropped after it sent its frame loses only its
    /// outcome; a call dropped while it sends ends the writer. A call takes the draft
    /// at its first poll, so after each result the writer holds no draft: the next
    /// write needs a new call of [`Writer::draft`].
    ///
    /// # Errors
    ///
    /// [`Error::Unsent`] for a frame of no series or with a group of no sample, or a
    /// frame of [`Time::Node`] that is encoded or holds more than one sample in a
    /// group, with nothing sent and the draft dropped; the writer goes on. Else the
    /// error that ended the writer, given again at each later call: the error that
    /// ended the renewal, or [`Error::Refused`] when the node stopped the stream
    /// (`BUSY` when the home has no block for the outcome of a frame, `MALFORMED` for
    /// a frame that breaks a rule of the wire), [`Error::Transport`] (a reset of
    /// `Code(0)` after a call dropped while it sent), and [`Error::Reply`] for a reply
    /// that `wire` refuses, a reply out of order, or a stream that ends early.
    ///
    /// # Panics
    ///
    /// When the writer holds no draft.
    pub async fn write(&mut self, time: Time) -> Result<Outcome, Error> {
        let frame = self
            .draft
            .take()
            .expect("a write sends the draft of a call of `Writer::draft`");
        self.check()?;
        let form = frame.form();
        let unsent = |count| {
            count == 0 || time == Time::Node && (form != Form::Raw || count != 1)
        };
        let mut counts = frame.ranges().map(|(_, range)| range.count);
        let first = counts.next().unwrap_or(0);
        if let Some(count) =
            iter::once(first).chain(counts).find(|&count| unsent(count))
        {
            return Err(Error::Unsent { count, form });
        }
        let written = self.written(time, frame).await;
        self.ending(written)
    }

    /// Waits until each frame that the home of a group took before the call is on
    /// disk. Gives at once when the homes took no frame. It never waits on a group
    /// whose write ended. A call dropped before it gave loses nothing: the next call
    /// takes the replies that it did not.
    ///
    /// # Errors
    ///
    /// The error that ended the writer, as [`Writer::write`] gives it, and
    /// [`Error::Unstored`] for each group whose write ended with a taken frame whose
    /// `Stored` did not come, so it may not be on disk; the writer goes on, and each
    /// later call gives it again.
    pub async fn stored(&mut self) -> Result<(), Error> {
        self.check()?;
        let mut waited = Ok(());
        while !self.replies.stored() {
            if let Err(error) = self.reply().await {
                waited = Err(error);
                break;
            }
        }
        self.ending(waited)?;
        self.unstored()
    }

    /// Drops a held draft, finishes the stream, and waits until each frame that the
    /// home took is on disk, or its group ended, and the node finished the stream.
    /// Control ends when the stream finishes. Dropping the call drops the writer.
    ///
    /// # Errors
    ///
    /// The error that ended the writer, as [`Writer::write`] gives it,
    /// [`Error::Reply`] with [`wire::hub::Error::Finished`] when the node finishes the
    /// stream before each group of each frame sent has its outcome, or before the
    /// `Stored` of each taken frame of a group that did not end, and then
    /// [`Error::Unstored`] as [`Writer::stored`] gives it.
    pub async fn finish(mut self) -> Result<(), Error> {
        self.draft = None;
        self.check()?;
        let finished = self.finished().await;
        self.ending(finished)?;
        self.unstored()
    }

    /// [`Error::Unstored`] for each group whose write ended with a taken frame whose
    /// `Stored` did not come.
    fn unstored(&self) -> Result<(), Error> {
        let groups: Vec<_> = self
            .replies
            .unstored()
            .map(|(group, end)| (self.set.groups()[to_usize(group)], end))
            .collect();
        if groups.is_empty() {
            Ok(())
        } else {
            Err(Error::Unstored(groups))
        }
    }

    /// Finishes the sender and takes each reply until the node finishes.
    async fn finished(&mut self) -> Result<(), Error> {
        let (sender, receiver) = self
            .stream
            .as_mut()
            .expect("invariant: a writer holds its stream until it ends");
        sender.finish()?;
        while let Some(message) = receiver.recv().await? {
            self.replies.decode(&message).map_err(Error::Reply)?;
        }
        if self.replies.stored() {
            Ok(())
        } else {
            Err(Error::Reply(wire::hub::Error::Finished))
        }
    }

    /// The error that ended the writer, and ends it when a call dropped while it sent.
    fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = &self.ended {
            return Err(error.clone());
        }
        if self.cut {
            // Dropping the stream resets it with `Code(0)`.
            let error = Error::Transport(transport::Error::Reset { code: Code(0) });
            return self.ending(Err(error));
        }
        Ok(())
    }

    /// `result`, and ends the writer when it is an error.
    fn ending<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if let Some(stream) = self.stream.take() {
            end(stream, &error);
        }
        let ended = self.session.0.ended.borrow().clone().unwrap_or(error);
        self.ended = Some(ended.clone());
        Err(ended)
    }

    /// Sends the head, the ends, the counts, and the body of `frame`. The body goes
    /// from the frame's own block.
    async fn send(&mut self, time: Time, frame: Draft) -> Result<(), Error> {
        let Self {
            session,
            stream,
            cut,
            ..
        } = self;
        let shared = &session.0;
        let (sender, _) = stream
            .as_mut()
            .expect("invariant: a writer holds its stream until it ends");
        let frame = frame.freeze(match time {
            Time::Frame(path) => path,
            Time::Node => Path::Live,
        });
        let lead = Lead::new(time, &frame);
        // The open checked that a block of the pool holds the lead.
        let mut bytes = shared.alloc(lead.bytes()).await?;
        lead.encode(&mut bytes);
        let bytes = bytes.freeze();
        let most = sender.bytes_max();
        *cut = true;
        for range in lead.messages(most) {
            send_range(sender, &bytes, range).await?;
        }
        let body = frame.body();
        for at in (0..body.len()).step_by(most) {
            send_range(sender, &body, at..body.len().min(at + most)).await?;
        }
        *cut = false;
        self.replies.sent(frame.ranges().map(|(group, _)| group));
        Ok(())
    }

    /// Sends `frame` and takes its outcome.
    async fn written(&mut self, time: Time, frame: Draft) -> Result<Outcome, Error> {
        self.send(time, frame).await?;
        self.outcome().await
    }

    /// Takes replies until the outcome of the last frame sent.
    async fn outcome(&mut self) -> Result<Outcome, Error> {
        loop {
            if let Reply::Outcome { outcome, .. } = self.reply().await?
                && self.replies.answered()
            {
                return Ok(outcome);
            }
        }
    }

    /// Takes the next reply, and counts it.
    async fn reply(&mut self) -> Result<Reply, Error> {
        let (_, receiver) = self
            .stream
            .as_mut()
            .expect("invariant: a writer holds its stream until it ends");
        let message = receiver
            .recv()
            .await?
            .ok_or(Error::Reply(wire::hub::Error::Finished))?;
        self.replies.decode(&message).map_err(Error::Reply)
    }
}

/// Sends `range` of `block` on `sender` as one message, with no copy.
async fn send_range(
    sender: &mut Sender,
    block: &block::Block,
    range: std::ops::Range<usize>,
) -> Result<(), Error> {
    let part = Part { range, zeros: 0 };
    Ok(sender.send_parts(block.clone(), &[part]).await?)
}

/// Sends the write of `open` and `open`, then takes the places and their run.
async fn start(
    session: &Handle,
    sender: &mut Sender,
    receiver: &mut Receiver,
    open: &Open,
) -> Result<Start, Error> {
    let mut body = vec![0; open.encoded_len()];
    open.encode(&mut body);
    session
        .0
        .open(
            sender,
            access::proof::Kind::Write,
            &body,
            |length, signature, out: &mut [u8; Write::LEN]| {
                Write { length, signature }.encode(out);
            },
        )
        .await?;
    let mut decoder = Opening::default();
    loop {
        let message = receiver.recv().await?;
        if let Some(start) = decoder.decode(message.as_deref())? {
            let placed = |name: &&Name| start.places.iter().any(|p| p.name == **name);
            return match open.channels.iter().find(|name| !placed(name)) {
                Some(name) => Err(Error::Unplaced(name.clone())),
                None => Ok(start),
            };
        }
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).expect("invariant: a u32 fits a usize")
}
