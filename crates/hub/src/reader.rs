//! Reader sessions: which frames one gets, why one does not open, and the session.

mod remote;

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use ::home::reader::Next;
use types::channel;
use types::frame::key_set::{Group, KeySet};
use types::frame::{Frame, Mask, View};
use types::name::Name;

use crate::{Away, State};
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
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(error) => write!(f, "the buffer of the shard failed: {error}"),
            Self::Behind => f.write_str(
                "the reader missed a frame and gets no later one: open a new reader",
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
        let mut keys = Vec::with_capacity(channels.len());
        let index = {
            let borrowed = state.borrow();
            let mut index = None;
            for name in channels {
                let channel = borrowed
                    .channels
                    .get(name)
                    .ok_or_else(|| Error::Unknown(name.clone()))?;
                if *index.get_or_insert(channel.index) != channel.index {
                    return Err(Error::ManyIndexes);
                }
                keys.push((channel.key, channel.data_type));
            }
            index.ok_or(Error::Empty)?
        };
        match crate::carry(state, index).await {
            Ok(()) => {}
            Err(Away::Remote(home, homes)) => {
                let mut held = types::hash::Set::default();
                held.insert(index);
                let data: Vec<_> = keys
                    .into_iter()
                    .filter(|&(key, _)| held.insert(key))
                    .collect();
                let group = Group { index, data: &data };
                let set = state.borrow_mut().interner.intern(&[group]);
                let remote = Remote::open(state, &homes, home, set, mode).await?;
                return Ok(Self {
                    source: Source::Remote(Box::new(remote)),
                    frame: None,
                });
            }
            Err(Away::Mesh(stopped)) => return Err(Error::Mesh(stopped)),
        }
        let (mut slots, slot) = {
            let mut borrowed = state.borrow_mut();
            let assigned = borrowed.interner.slots();
            let slots: Vec<_> = keys
                .into_iter()
                .map(|(key, _)| assigned.assign(key))
                .collect();
            (slots, assigned.assign(index))
        };
        // A frame without the reader's channels still shows that time moved.
        slots.push(slot);
        let slots = slots.into();
        let (session, credit) = match mode {
            Mode::Complete => {
                let (session, credit) = Session::complete(
                    state,
                    slots,
                    slot,
                    WINDOW,
                    ::home::reader::complete::Charge::Whole,
                );
                (session, Some((credit, 0)))
            }
            Mode::Latest => (Session::latest(state, slots, slot), None),
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
    /// [`Ended`] once no frame waits and the session can give no more, on this and
    /// every later call: [`Ended::Behind`] before [`Ended::Buffer`].
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
}

impl From<Stop> for Ended {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Buffer(error) => Self::Buffer(error),
            Stop::Behind => Self::Behind,
        }
    }
}

/// A session at the shard's home, through a mask of the reader's channels: the frames
/// it takes, and why it ends. Each [`Reader`] drives one, and so does each stream of a
/// remote reader.
#[derive(Debug)]
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The key set of the last frame, and the mask of the reader's channels in it.
    mask: Option<(Arc<KeySet>, Mask)>,
    /// Frames given in a row since `take` last returned `Pending`.
    streak: u32,
}

impl Session {
    /// Opens a complete session through `slots` on the index of `index`, with a grant
    /// of `limit_bytes` that each frame spends as `charge` says. Returns the session
    /// and the credit that raises its grant.
    pub(crate) fn complete(
        state: &Rc<RefCell<State>>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
        limit_bytes: u64,
        charge: ::home::reader::complete::Charge,
    ) -> (Self, Credit) {
        let key = state
            .borrow_mut()
            .home
            .open_complete(index, limit_bytes, charge);
        let credit = Credit {
            state: Rc::clone(state),
            key,
        };
        (Self::new(state, key.into(), slots), credit)
    }

    /// Opens a latest session through `slots` on the index of `index`.
    pub(crate) fn latest(
        state: &Rc<RefCell<State>>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
    ) -> Self {
        let key = state.borrow_mut().home.open_latest(index);
        Self::new(state, key, slots)
    }

    fn new(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::Key,
        slots: Box<[channel::Slot]>,
    ) -> Self {
        Self {
            state: Rc::clone(state),
            key,
            slots,
            mask: None,
            streak: 0,
        }
    }

    /// The next frame, its key set, and the mask of the reader's channels in it. After
    /// [`STREAK`] frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// [`Stop`] once no frame waits and the session can give no more, on this and
    /// every later call: [`Stop::Behind`] before [`Stop::Buffer`].
    ///
    /// # Panics
    ///
    /// When the interner does not hold the key set of the frame, which a writer of
    /// this hub made.
    pub(crate) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Stop> {
        let frame = poll_fn(|cx| {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            if self.streak == STREAK {
                self.streak = 0;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            match state.home.take(self.key) {
                Next::Frame(frame) => {
                    self.streak += 1;
                    return Poll::Ready(Ok(frame));
                }
                Next::Behind => return Poll::Ready(Err(Stop::Behind)),
                Next::Empty => {}
            }
            if let Some(error) = &state.failed {
                return Poll::Ready(Err(Stop::Buffer(error.clone())));
            }
            self.streak = 0;
            state.wakers.insert(self.key, cx.waker().clone());
            Poll::Pending
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

/// The credit of a complete [`Session`]: only a complete open gives one.
#[derive(Debug)]
pub(crate) struct Credit {
    state: Rc<RefCell<State>>,
    key: ::home::reader::complete::Key,
}

impl Credit {
    /// Raises the grant of the session to `limit_bytes` since the open.
    pub(crate) fn grant(&self, limit_bytes: u64) {
        self.state.borrow_mut().home.grant(self.key, limit_bytes);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.wakers.remove(&self.key);
        state.home.close_reader(self.key);
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
