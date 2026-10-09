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
use types::name::Name;
use types::sample::Type;

use crate::{Away, Removal, State};
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

/// One frame that a reader got, through the reader's mask (M2): only the reader's
/// channels and their index.
#[derive(Debug)]
pub struct Received<'a> {
    /// The frame through the mask. Its series are encoded.
    pub view: View<'a>,
    /// The key set that the view's entries index.
    pub set: &'a Arc<KeySet>,
}

/// Why a reader session gives no more frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The shard's buffer failed.
    Buffer(env::files::Error),
    /// A complete reader missed a frame ([`Mode::Complete`]).
    Behind,
    /// A channel of the reader was removed from the definitions.
    Removed(channel::Key),
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
    /// No channel has this name.
    Unknown(Name),
    /// The channels are on more than one index.
    ManyIndexes,
    /// The reader names no channel.
    Empty,
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
            Self::Unknown(name) => write!(f, "no channel is named {name}"),
            Self::ManyIndexes => f.write_str(
                "the channels are on more than one index: open a reader per index",
            ),
            Self::Empty => f.write_str("a reader names at least one channel"),
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

/// A reader session through the reader's channels. Dropping it closes the session;
/// frames that wait do not go out.
#[derive(Debug)]
pub struct Reader {
    source: Source,
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

/// A reader session at this node's home, with the credit of a complete one and the
/// charge of each frame it gave back.
#[derive(Debug)]
struct Local {
    session: Session,
    credit: Option<(Credit, u64)>,
}

impl Local {
    /// Raises the grant by the charge of `frame`, which the reader gave back.
    fn give_back(&mut self, frame: &Frame) {
        if let Some((credit, taken_bytes)) = &mut self.credit {
            *taken_bytes += frame.charge();
            credit.grant(*taken_bytes + WINDOW);
        }
    }
}

impl Reader {
    /// Opens a reader on the index of `channels`.
    pub(crate) async fn open(
        state: &Rc<RefCell<State>>,
        channels: &[Name],
        mode: Mode,
    ) -> Result<Self, Error> {
        let (keys, index) = loop {
            let (_, index) = resolve(&state.borrow(), channels)?;
            let away = match crate::homes(state, &[index]).await {
                Ok(()) => None,
                Err(Away::Remote(home)) => Some(home),
                Err(Away::Mesh(stopped)) => return Err(Error::Mesh(stopped)),
            };
            // A call of `set_definitions` while the open waits can change a channel.
            let (keys, again) = resolve(&state.borrow(), channels)?;
            if again != index {
                continue;
            }
            if let Some(home) = away {
                let remote = Remote::open(state, home, keys, index, mode).await?;
                return Ok(Self {
                    source: Source::Remote(Box::new(remote)),
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
        let keys = keys.into();
        let (session, credit) = match mode {
            Mode::Complete => {
                let (session, credit) = Session::complete(
                    state,
                    keys,
                    slots,
                    slot,
                    WINDOW,
                    ::home::reader::complete::Charge::Whole,
                );
                (session, Some((credit, 0)))
            }
            Mode::Latest => (Session::latest(state, keys, slots, slot), None),
        };
        Ok(Self {
            source: Source::Local(Local { session, credit }),
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
    /// [`Ended::Removed`] once a channel of the reader is removed, before any frame
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
            let (frame, set, mask) = match &mut self.source {
                Source::Local(local) => local.session.take().await?,
                Source::Remote(remote) => remote.take().await?,
            };
            let frame = self.frame.insert(frame);
            Ok(Received {
                view: View::new(frame, mask),
                set,
            })
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
}

impl From<Stop> for Ended {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Buffer(error) => Self::Buffer(error),
            Stop::Behind => Self::Behind,
            Stop::Removed(key) => Self::Removed(key),
        }
    }
}

/// The key and sample type of each of `channels`, and their one index.
fn resolve(
    state: &State,
    channels: &[Name],
) -> Result<(Vec<(channel::Key, Type)>, channel::Key), Error> {
    let mut keys = Vec::with_capacity(channels.len());
    let mut index = None;
    for name in channels {
        let channel = state
            .channels
            .named(name)
            .ok_or_else(|| Error::Unknown(name.clone()))?;
        if *index.get_or_insert(channel.index()) != channel.index() {
            return Err(Error::ManyIndexes);
        }
        keys.push((channel.key(), channel.sample()));
    }
    Ok((keys, index.ok_or(Error::Empty)?))
}

/// A session at the shard's home, through a mask of the reader's channels: the frames
/// it takes, and why it ends. Each [`Reader`] drives one, and so does each stream of a
/// remote reader.
#[derive(Debug)]
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// The channel whose removal ended the session.
    removed: Removal,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The key set of the last frame, and the mask of the reader's channels in it.
    mask: Option<(Arc<KeySet>, Mask)>,
    streak: Streak,
}

impl Session {
    /// Opens a complete session on `keys` through their `slots` on the index of
    /// `index`, with a grant of `limit_bytes` that each frame spends as `charge` says.
    /// Returns the session and the credit that raises its grant.
    pub(crate) fn complete(
        state: &Rc<RefCell<State>>,
        keys: Box<[channel::Key]>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
        limit_bytes: u64,
        charge: ::home::reader::complete::Charge,
    ) -> (Self, Credit) {
        let key = state
            .borrow_mut()
            .home
            .open_complete(index, limit_bytes, charge);
        let session = Self::new(state, key.into(), keys, slots);
        let credit = Credit {
            state: Rc::clone(state),
            key,
            removed: session.removed.clone(),
        };
        (session, credit)
    }

    /// Opens a latest session on `keys` through their `slots` on the index of `index`.
    pub(crate) fn latest(
        state: &Rc<RefCell<State>>,
        keys: Box<[channel::Key]>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
    ) -> Self {
        let key = state.borrow_mut().home.open_latest(index);
        Self::new(state, key, keys, slots)
    }

    fn new(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::Key,
        keys: Box<[channel::Key]>,
        slots: Box<[channel::Slot]>,
    ) -> Self {
        let removed = state.borrow_mut().readers.add(key, keys);
        Self {
            state: Rc::clone(state),
            key,
            removed,
            slots,
            mask: None,
            streak: Streak::default(),
        }
    }

    /// The next frame, its key set, and the mask of the reader's channels in it. After
    /// [`STREAK`] frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// [`Stop::Removed`] once a channel of the session is removed, before any frame
    /// that waits. Else [`Stop`] once no frame waits and the session can give no more.
    /// Either on this and every later call: [`Stop::Behind`] before [`Stop::Buffer`].
    ///
    /// # Panics
    ///
    /// When the interner does not hold the key set of the frame, which a writer of
    /// this hub made.
    pub(crate) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Stop> {
        let frame = poll_fn(|cx| {
            if let Some(key) = self.removed.get() {
                return Poll::Ready(Err(Stop::Removed(key)));
            }
            ready!(self.streak.poll(cx));
            let polled = poll_take(&mut self.state.borrow_mut(), self.key, cx);
            self.streak.count(&polled);
            polled
        })
        .await?;
        let key = frame.key_set();
        let (set, mask) = match self.mask.take() {
            Some(mask) if mask.0.key() == key => self.mask.insert(mask),
            _ => {
                let snapshot = self.state.borrow().interner.snapshot();
                let set = snapshot
                    .get(key)
                    .expect("invariant: a frame's key set is known");
                let mask = Mask::new(set, self.slots.iter().copied());
                self.mask.insert((Arc::clone(set), mask))
            }
        };
        Ok((frame, set, mask))
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

/// The credit of a complete [`Session`]: only a complete open gives one.
#[derive(Debug)]
pub(crate) struct Credit {
    state: Rc<RefCell<State>>,
    key: ::home::reader::complete::Key,
    removed: Removal,
}

impl Credit {
    /// Raises the grant of the session to `limit_bytes` since the open. Changes
    /// nothing once the session ended on a removed channel.
    pub(crate) fn grant(&self, limit_bytes: u64) {
        if self.removed.get().is_none() {
            self.state.borrow_mut().home.grant(self.key, limit_bytes);
        }
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
