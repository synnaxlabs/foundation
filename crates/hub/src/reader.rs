//! Reader sessions: which frames one gets, why one does not open, and the session.

pub(crate) mod remote;

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use ::home::reader::Next;
use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{Frame, Mask, View};
use types::name::{Name, Selector};
use types::sample::Type;
use types::time::Span;

use crate::channel::Channel;
use crate::{Away, End, Ending, State};
use remote::Remote;

/// The credit a complete reader has past the frames it gave back: a fixed window until
/// the hub sizes it from the link.
const WINDOW: u64 = 1 << 20;
/// Frames that [`Reader::next`] gives in a row before it yields once.
const STREAK: u32 = 128;

/// Which frames a reader gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Each live frame, after the commit that holds it. Once the frames that the
    /// reader has not given back (the one it holds and those it has not taken) reach
    /// a window, a later frame waits until the reader gives frames back. A frame that
    /// still waits when the hub releases the next commit with frames of the index is
    /// a miss: the session ends with [`Ended::Behind`] after the frames before it. So
    /// a reader that takes the frames of each commit before the next such commit ends
    /// misses none.
    Complete,
    /// The newest live frame, before its commit.
    Latest,
}

/// What a reader session reads, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The channels it reads: each channel the hub knows at the open whose name the
    /// selector matches.
    pub select: Selector,
    /// Which frames it gets.
    pub mode: Mode,
    /// The subject that opens the reader. A named reader belongs to it: an open by the
    /// same subject and name on the same index takes over the session, and an open by
    /// another subject opens another reader.
    pub subject: Name,
    /// The reader's name, or `None`. A named reader has at most one session on each
    /// index: an open takes over the session of the same subject and name on its index.
    /// A named complete reader that opens while the home holds its position resumes at
    /// the position that the acks of its last complete session recorded, or where that
    /// session opened when they recorded none. It ends with [`Ended::Behind`] when a
    /// frame after that position was released, or dropped because no complete session
    /// on its index was open.
    pub name: Option<Name>,
    /// How long the home holds a named complete reader's position after its session
    /// closes. Zero or more. It must be zero when the reader is unnamed or latest.
    pub hold: Span,
}

/// One frame that a reader got, through the reader's mask (M2): only the reader's
/// channels and their index.
#[derive(Debug)]
pub struct Received<'a> {
    view: View<'a>,
    set: &'a Arc<KeySet>,
    position: Position,
}

impl<'a> Received<'a> {
    /// The frame through the mask. Its series are encoded.
    #[must_use]
    pub fn view(&self) -> View<'a> {
        self.view
    }

    /// The key set that the view's entries index.
    #[must_use]
    pub fn set(&self) -> &'a Arc<KeySet> {
        self.set
    }

    /// The reader's position after this frame. Give it to [`Reader::ack`] when the
    /// frame is safe at its target.
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }
}

/// A position on one index: a reader at it has each sample below it. Only
/// [`Received`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    index: channel::Slot,
    /// The seq of the first live sample that the reader has not received.
    live: u64,
}

impl Position {
    /// The position of a reader on the index at `index` after `frame`, whose key set
    /// `lens` holds.
    fn after(frame: &Frame, lens: &Lens, index: channel::Slot) -> Self {
        let range = frame
            .range(lens.group)
            .expect("invariant: a frame holds the range of each group");
        Self {
            index,
            live: range.seq + u64::from(range.count),
        }
    }
}

/// A key set, the mask of a reader's channels in it, and the group of the reader's
/// index in it.
#[derive(Debug)]
pub(crate) struct Lens {
    pub(crate) set: Arc<KeySet>,
    pub(crate) mask: Mask,
    group: u32,
}

impl Lens {
    /// The lens of `set` for a reader of `slots` on the index at `index`.
    ///
    /// # Panics
    ///
    /// If `set` does not hold `index`.
    pub(crate) fn new(
        set: &Arc<KeySet>,
        slots: impl IntoIterator<Item = channel::Slot>,
        index: channel::Slot,
    ) -> Self {
        let entry = set
            .find(index)
            .expect("invariant: the key set of a frame on an index holds the index");
        Self {
            set: Arc::clone(set),
            mask: Mask::new(set, slots),
            group: set.entries()[entry].group,
        }
    }
}

/// Why a reader session gives no more frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The shard's buffer failed.
    Buffer(env::files::Error),
    /// A complete reader missed a frame: [`Mode::Complete`] and [`Config::name`] state
    /// when.
    Behind,
    /// A channel of the reader was removed from the definitions.
    Removed(channel::Key),
    /// A later open of the same subject and name took over the session.
    Replaced,
    /// The stream to the home of another node broke.
    Stream(transport::Error),
    /// The home of another node stopped or reset the stream with this HUB WIRE code.
    Refused(wire::hub::Refusal),
    /// A message from the home of another node broke HUB WIRE. The reader stopped the
    /// stream with code `MALFORMED`.
    Message(wire::hub::Error),
    /// The ends of a frame from the home of another node break a rule of a frame. The
    /// reader stopped the stream with code `MALFORMED`.
    Frame(types::frame::Error),
    /// The shard's pool had no block for a frame or a credit. The reader stopped the
    /// stream with code `BUSY`.
    Pool(block::Error),
    /// The home of another node started a frame once the charges of the frames that
    /// arrived reached the grant that the reader sent. The reader stopped the stream
    /// with code `MALFORMED`.
    Credit {
        /// The grant that the reader sent.
        limit_bytes: u64,
    },
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(error) => write!(f, "the buffer of the shard failed: {error}"),
            Self::Behind => f.write_str(
                "the reader missed a frame and gets no later one: open a new reader",
            ),
            Self::Removed(key) => {
                write!(f, "channel {key} was removed: open a new reader")
            }
            Self::Replaced => f.write_str(
                "a later open of the same named reader took over the session",
            ),
            Self::Stream(error) => write!(f, "the stream to the home broke: {error}"),
            Self::Refused(refusal) => write!(
                f,
                "the home ended the session with code {}: {refusal}",
                refusal.code()
            ),
            Self::Message(error) => {
                write!(f, "a message from the home broke the hub protocol: {error}")
            }
            Self::Frame(error) => {
                write!(f, "a frame from the home is not valid: {error}")
            }
            Self::Pool(error) => {
                write!(f, "the pool had no block for the reader: {error}")
            }
            Self::Credit { limit_bytes } => write!(
                f,
                "the home sent a frame past the credit of {limit_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for Ended {}

/// Why a reader session did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The selector matches no channel.
    Empty,
    /// The channels are on more than one index: `first` is the least matched name,
    /// and `other` the least matched name on another index.
    ManyIndexes {
        /// The least matched name.
        first: Name,
        /// The least matched name on another index than `first`.
        other: Name,
    },
    /// The reader is named and the node has no mesh time yet. Open it again later.
    Unsynced,
    /// The reader is named, and the home of its index is `home`, another node. A named
    /// reader opens only at the home of its index.
    Remote {
        /// The home.
        home: types::node::Key,
    },
    /// The mesh stopped, so the home of the index is not known.
    Mesh(mesh::Stopped),
    /// The session or the stream to the home of another node failed.
    Transport(transport::Error),
    /// The home of another node stopped or reset the stream with this HUB WIRE code.
    Refused(wire::hub::Refusal),
    /// The reply of the home of another node broke HUB WIRE. The reader stopped the
    /// stream with code `MALFORMED`.
    Message(wire::hub::Error),
    /// The shard's pool had no block for a message to the home of another node. The
    /// reader stopped the stream with code `BUSY`.
    Pool(block::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the selector matches no channel"),
            Self::ManyIndexes { first, other } => write!(
                f,
                "the channels {first} and {other} are on different indexes: open a \
                 reader per index"
            ),
            Self::Unsynced => f.write_str(
                "the node has no mesh time yet: open the named reader again later",
            ),
            Self::Remote { home } => write!(
                f,
                "the home of the index of the reader is node {home}, and a named reader \
                 opens only at this node"
            ),
            Self::Mesh(stopped) => write!(f, "the mesh stopped: {stopped}"),
            Self::Transport(error) => {
                write!(f, "the transport to the home failed: {error}")
            }
            Self::Refused(refusal) => write!(
                f,
                "the home refused the reader with code {}: {refusal}",
                refusal.code()
            ),
            Self::Message(error) => {
                write!(f, "the reply of the home broke the hub protocol: {error}")
            }
            Self::Pool(error) => {
                write!(f, "the pool had no block for the open: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<::home::reader::Unsynced> for Error {
    fn from(::home::reader::Unsynced: ::home::reader::Unsynced) -> Self {
        Self::Unsynced
    }
}

/// A reader session through the reader's channels. Dropping it closes the session;
/// frames that wait do not go out.
#[derive(Debug)]
pub struct Reader {
    source: Source,
    /// The slot of the reader's index.
    index: channel::Slot,
    /// The `live` of the position of the last [`Received`] that `next` gave, or 0.
    given: u64,
    /// The frame that the last [`Received`] lends.
    frame: Option<Frame>,
}

/// Where a reader's frames come from.
#[derive(Debug)]
enum Source {
    /// A session at this node's home.
    Local(Local),
    /// A session at the home of another node.
    Remote(Box<Remote>),
}

/// A reader session at this node's home, with the grant and ack of a complete one, and
/// the charge of each frame it gave back.
#[derive(Debug)]
struct Local {
    session: Session,
    /// `None` for a latest reader.
    complete: Option<(Complete, u64)>,
}

impl Local {
    /// Opens a session on `channels` at this node, the home of their index.
    fn open(
        state: &Rc<RefCell<State>>,
        channels: Channels,
        mode: Mode,
        named: Option<::home::reader::named::Key>,
        hold: Span,
    ) -> Result<Self, Error> {
        let charge = ::home::reader::complete::Charge::Whole;
        let (session, complete) = match (mode, named) {
            (Mode::Complete, None) => {
                let (session, complete) =
                    Session::complete(state, channels, WINDOW, charge);
                (session, Some(complete))
            }
            (Mode::Complete, Some(named)) => {
                let key = {
                    let mut state = state.borrow_mut();
                    let opened = state.home.open_named_complete(
                        channels.index,
                        named,
                        hold,
                        WINDOW,
                        charge,
                    )?;
                    state.take_over(opened)
                };
                let (session, complete) = Session::with_complete(state, key, channels);
                (session, Some(complete))
            }
            (Mode::Latest, None) => (Session::latest(state, channels), None),
            (Mode::Latest, Some(named)) => {
                let key = {
                    let mut state = state.borrow_mut();
                    let opened = state.home.open_named_latest(channels.index, named)?;
                    state.take_over(opened)
                };
                (Session::new(state, key, channels), None)
            }
        };
        Ok(Self {
            session,
            complete: complete.map(|complete| (complete, 0)),
        })
    }

    /// Raises the grant by the charge of `frame`, which the reader gave back.
    fn give_back(&mut self, frame: &Frame) {
        if let Some((complete, taken_bytes)) = &mut self.complete {
            *taken_bytes += frame.charge();
            complete.grant(*taken_bytes + WINDOW);
        }
    }
}

impl Reader {
    /// Opens a reader on the channels that `config.select` matches.
    ///
    /// # Panics
    ///
    /// When `config.hold` is negative, or not zero for an unnamed or latest reader.
    pub(crate) async fn open(
        state: &Rc<RefCell<State>>,
        config: Config,
    ) -> Result<Self, Error> {
        let Config {
            select,
            mode,
            subject,
            name,
            hold,
        } = config;
        assert!(
            hold >= Span::ZERO,
            "the hold {hold} of a reader is negative"
        );
        assert!(
            hold == Span::ZERO || (name.is_some() && mode == Mode::Complete),
            "a hold of {hold} for a reader that is unnamed or latest"
        );
        let named = name.map(|name| ::home::reader::named::Key { subject, name });
        let (keys, index) = loop {
            let (_, index) = resolve(&state.borrow(), &select)?;
            let away = match crate::homes(state, &[index]).await {
                Ok(()) => None,
                Err(Away::Remote(home)) => Some(home),
                Err(Away::Mesh(stopped)) => return Err(Error::Mesh(stopped)),
            };
            // A call of `set_definitions` while the open waits can change a channel.
            let (keys, again) = resolve(&state.borrow(), &select)?;
            if again != index {
                continue;
            }
            if let Some(home) = away {
                if named.is_some() {
                    return Err(Error::Remote { home });
                }
                let (remote, slot) =
                    Remote::open(state, home, keys, index, mode).await?;
                return Ok(Self {
                    index: slot,
                    source: Source::Remote(Box::new(remote)),
                    given: 0,
                    frame: None,
                });
            }
            break (keys, index);
        };
        let mut keys: Vec<_> = keys.into_iter().map(|(key, _)| key).collect();
        // A frame without the reader's channels still shows that time moved.
        keys.push(index);
        let slots = state.borrow_mut().slots(index, &keys);
        let slot = slots[slots.len() - 1];
        let channels = Channels {
            keys: keys.into(),
            slots,
            index: slot,
        };
        let local = Local::open(state, channels, mode, named, hold)?;
        Ok(Self {
            source: Source::Local(local),
            index: slot,
            given: 0,
            frame: None,
        })
    }

    /// The next frame, as a view of the reader's channels. The view borrows the
    /// reader, so the frame stays in use, and spends the reader's credit, until the
    /// next call, not its first poll, or the drop. After a run of frames, it yields
    /// once, so a task that loops on it lets the shard's other tasks run.
    ///
    /// # Errors
    ///
    /// [`Ended::Removed`] once a channel of the reader is removed, or
    /// [`Ended::Replaced`] once a later open takes over the session, before any frame
    /// that waits. Else [`Ended`] once no frame waits and the session can give no
    /// more. Either on this and every later call: [`Ended::Behind`] before
    /// [`Ended::Buffer`].
    #[expect(
        clippy::should_implement_trait,
        reason = "it gives a future, which `Iterator::next` cannot"
    )]
    pub fn next(&mut self) -> impl Future<Output = Result<Received<'_>, Ended>> {
        if let Some(frame) = self.frame.take() {
            match &mut self.source {
                Source::Local(local) => local.give_back(&frame),
                Source::Remote(remote) => remote.give_back(&frame),
            }
        }
        async move {
            let (frame, lens) = match &mut self.source {
                Source::Local(local) => local.session.take().await?,
                Source::Remote(remote) => remote.take().await?,
            };
            let frame = self.frame.insert(frame);
            let position = Position::after(frame, lens, self.index);
            self.given = position.live;
            Ok(Received {
                position,
                view: View::new(frame, &lens.mask),
                set: &lens.set,
            })
        }
    }

    /// Records that the reader has each sample up to `position`. A named complete
    /// reader that opens again starts there. An ack at or below the reader's last ack
    /// changes nothing. Nor does the ack of a latest reader, which holds nothing, of a
    /// reader whose index has its home at another node, or of a reader that ended:
    /// after `next` gave an [`Ended`], or once the hub ended it with
    /// [`Ended::Removed`] or [`Ended::Replaced`].
    ///
    /// # Panics
    ///
    /// If `position` is of another index than the reader's, or is past the position
    /// of the last [`Received`] that `next` gave. Before `next` gave one, each
    /// `position` panics.
    pub fn ack(&mut self, position: Position) {
        assert!(
            position.index == self.index,
            "the position is of another index than the reader's"
        );
        assert!(
            position.live <= self.given,
            "the position is past the last frame that this reader gave: live {} past {}",
            position.live,
            self.given
        );
        match &self.source {
            Source::Local(Local {
                complete: Some((complete, _)),
                ..
            }) => complete.ack(position.live),
            Source::Local(_) | Source::Remote(_) => {}
        }
    }
}

/// Why a [`Session`] at this node's home gives no more frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The shard's buffer failed.
    Buffer(env::files::Error),
    /// A complete session missed a frame.
    Behind,
    /// A channel of the session was removed from the definitions.
    Removed(channel::Key),
    /// A later open of the same named reader took over the session.
    Replaced,
}

impl From<End> for Stop {
    fn from(end: End) -> Self {
        match end {
            End::Removed(key) => Self::Removed(key),
            End::Replaced => Self::Replaced,
        }
    }
}

impl From<Stop> for Ended {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Buffer(error) => Self::Buffer(error),
            Stop::Behind => Self::Behind,
            Stop::Removed(key) => Self::Removed(key),
            Stop::Replaced => Self::Replaced,
        }
    }
}

/// The key and sample type of each channel that `select` matches, in name order, and
/// their one index.
fn resolve(
    state: &State,
    select: &Selector,
) -> Result<(Vec<(channel::Key, Type)>, channel::Key), Error> {
    let mut matched: Vec<(&Name, &Channel)> = state
        .channels
        .iter()
        .filter(|&(name, _)| select.matches(name).is_some())
        .collect();
    matched.sort_unstable_by_key(|&(name, _)| name);
    let &(first, channel) = matched.first().ok_or(Error::Empty)?;
    let index = channel.index();
    if let Some(&(other, _)) = matched.iter().find(|(_, other)| other.index() != index)
    {
        return Err(Error::ManyIndexes {
            first: first.clone(),
            other: other.clone(),
        });
    }
    let keys = matched
        .iter()
        .map(|(_, channel)| (channel.key(), channel.sample()))
        .collect();
    Ok((keys, index))
}

/// A session at the shard's home, through a mask of the reader's channels: the frames
/// it takes, and why it ends. Each [`Reader`] drives one, and so does each stream of a
/// remote reader.
#[derive(Debug)]
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// Why the hub ended the session.
    ending: Ending,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The slot of their index.
    index: channel::Slot,
    /// The lens of the key set of the last frame.
    lens: Option<Lens>,
    streak: Streak,
}

/// The channels of a [`Session`]: the key and slot of each, and the slot of their
/// index.
#[derive(Debug)]
pub(crate) struct Channels {
    pub(crate) keys: Box<[channel::Key]>,
    pub(crate) slots: Box<[channel::Slot]>,
    pub(crate) index: channel::Slot,
}

impl Session {
    /// Opens a complete session on `channels`, with a grant of `limit_bytes` that each
    /// frame spends as `charge` says. Returns the session and its grant and ack.
    pub(crate) fn complete(
        state: &Rc<RefCell<State>>,
        channels: Channels,
        limit_bytes: u64,
        charge: ::home::reader::complete::Charge,
    ) -> (Self, Complete) {
        let key =
            state
                .borrow_mut()
                .home
                .open_complete(channels.index, limit_bytes, charge);
        Self::with_complete(state, key, channels)
    }

    /// Opens a latest session on `channels`.
    pub(crate) fn latest(state: &Rc<RefCell<State>>, channels: Channels) -> Self {
        let key = state.borrow_mut().home.open_latest(channels.index);
        Self::new(state, key, channels)
    }

    fn with_complete(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::complete::Key,
        channels: Channels,
    ) -> (Self, Complete) {
        let session = Self::new(state, key.into(), channels);
        let complete = Complete {
            state: Rc::clone(state),
            key,
            ending: session.ending.clone(),
        };
        (session, complete)
    }

    fn new(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::Key,
        channels: Channels,
    ) -> Self {
        let ending = state.borrow_mut().readers.add(key, channels.keys);
        Self {
            state: Rc::clone(state),
            key,
            ending,
            slots: channels.slots,
            index: channels.index,
            lens: None,
            streak: Streak::default(),
        }
    }

    /// The next frame, and the lens of its key set. After [`STREAK`] frames in a row,
    /// it yields once.
    ///
    /// # Errors
    ///
    /// [`Stop::Removed`] once a channel of the session is removed, or
    /// [`Stop::Replaced`] once a takeover ends it, before any frame that waits. Else
    /// [`Stop`] once no frame waits and the session can give no more. Either on this
    /// and every later call: [`Stop::Behind`] before [`Stop::Buffer`].
    ///
    /// # Panics
    ///
    /// When the interner does not hold the key set of the frame, which a writer of
    /// this hub made.
    pub(crate) async fn take(&mut self) -> Result<(Frame, &Lens), Stop> {
        let frame = poll_fn(|cx| {
            if let Some(end) = self.ending.get() {
                return Poll::Ready(Err(end.into()));
            }
            ready!(self.streak.poll(cx));
            let polled = poll_take(&mut self.state.borrow_mut(), self.key, cx);
            self.streak.count(&polled);
            polled
        })
        .await?;
        let key = frame.key_set();
        let lens = match self.lens.take() {
            Some(lens) if lens.set.key() == key => self.lens.insert(lens),
            _ => {
                let snapshot = self.state.borrow().interner.snapshot();
                let set = snapshot
                    .get(key)
                    .expect("invariant: a frame's key set is known");
                let lens = Lens::new(set, self.slots.iter().copied(), self.index);
                self.lens.insert(lens)
            }
        };
        Ok((frame, lens))
    }
}

/// The next frame of the session `key`, or the waker of `cx` kept to wake once one
/// waits.
fn poll_take(
    state: &mut State,
    key: ::home::reader::Key,
    cx: &Context<'_>,
) -> Poll<Result<Frame, Stop>> {
    match state.home.take(key) {
        Next::Frame(frame) => return Poll::Ready(Ok(frame)),
        Next::Behind => return Poll::Ready(Err(Stop::Behind)),
        Next::Empty => {}
    }
    if let Some(error) = &state.failed {
        return Poll::Ready(Err(Stop::Buffer(error.clone())));
    }
    state.wakers.insert(key, cx.waker().clone());
    Poll::Pending
}

/// The frames that a source gave in a row since it last waited.
#[derive(Debug, Default)]
pub(super) struct Streak(u32);

impl Streak {
    /// `Pending` once, with a wake, after [`STREAK`] frames in a row, so the shard's
    /// other tasks run; else `Ready`.
    fn poll(&mut self, cx: &Context<'_>) -> Poll<()> {
        if self.0 < STREAK {
            return Poll::Ready(());
        }
        self.0 = 0;
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Counts one poll of the source: a wait starts the count again.
    fn count<T>(&mut self, polled: &Poll<T>) {
        self.0 = if polled.is_pending() { 0 } else { self.0 + 1 };
    }
}

/// The grant and the ack of a complete [`Session`]: only a complete open gives one.
#[derive(Debug)]
pub(crate) struct Complete {
    state: Rc<RefCell<State>>,
    key: ::home::reader::complete::Key,
    ending: Ending,
}

impl Complete {
    /// Raises the grant of the session to `limit_bytes` since the open. Changes
    /// nothing once the hub ended the session.
    pub(crate) fn grant(&self, limit_bytes: u64) {
        if self.ending.get().is_none() {
            self.state.borrow_mut().home.grant(self.key, limit_bytes);
        }
    }

    /// Records that the session has each sample below seq `live`. Changes nothing
    /// once the hub ended the session.
    fn ack(&self, live: u64) {
        if self.ending.get().is_some() {
            return;
        }
        let position = ::home::reader::Position {
            live,
            backfill: None,
        };
        self.state
            .borrow_mut()
            .home
            .ack(self.key, position)
            .expect("invariant: a hub position keeps the reader's paths");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.state.borrow_mut().close_reader(self.key);
    }
}

#[cfg(test)]
mod tests {
    use wire::hub::Refusal;

    use super::*;

    #[test]
    fn names_the_code_of_a_refusal_and_what_the_home_meant() {
        assert_eq!(
            Error::Refused(Refusal::NotHome).to_string(),
            "the home refused the reader with code 17: the node is not the home of \
             the index"
        );
        assert_eq!(
            Ended::Refused(Refusal::Failed).to_string(),
            "the home ended the session with code 18: the home's buffer failed, or \
             its mesh stopped"
        );
    }
}
