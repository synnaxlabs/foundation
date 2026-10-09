//! Reader sessions: which frames one gets, why one does not open, and the session.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use ::home::reader::Next;
use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{Frame, Mask, View};
use types::name::Name;

use crate::{Away, Removal, State};

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
    /// The home of the index is `home`, another node, and this hub does not yet read
    /// from another node (#340).
    Remote {
        /// The home.
        home: types::node::Key,
    },
    /// The mesh stopped, so the home of the index is not known.
    Mesh(mesh::Stopped),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no channel is named {name}"),
            Self::ManyIndexes => f.write_str(
                "the channels are on more than one index: open a reader per index",
            ),
            Self::Empty => f.write_str("a reader names at least one channel"),
            Self::Remote { home } => write!(
                f,
                "the home of the index is node {home}, and a reader reads only at \
                 this node"
            ),
            Self::Mesh(stopped) => write!(f, "the mesh stopped: {stopped}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Away> for Error {
    fn from(away: Away) -> Self {
        match away {
            Away::Remote(home) => Self::Remote { home },
            Away::Mesh(stopped) => Self::Mesh(stopped),
        }
    }
}

/// A reader session through the reader's channels. Dropping it closes the session;
/// frames that wait do not go out.
#[derive(Debug)]
pub struct Reader {
    session: Session,
    /// The credit of a complete reader, and the charge of each frame it took and gave
    /// back.
    credit: Option<(Credit, u64)>,
    /// The frame that the last [`Received`] lends.
    frame: Option<Frame>,
}

impl Reader {
    /// Opens a reader on the index of `channels`.
    pub(crate) async fn open(
        state: &Rc<RefCell<State>>,
        channels: &[Name],
        mode: Mode,
    ) -> Result<Self, Error> {
        let (mut keys, index) = loop {
            let (_, index) = resolve(&state.borrow(), channels)?;
            crate::home(state, index).await?;
            // A call of `set_definitions` while the open waits can change a channel.
            let (keys, again) = resolve(&state.borrow(), channels)?;
            if again == index {
                break (keys, index);
            }
        };
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
            session,
            credit,
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
        if let Some(frame) = self.frame.take()
            && let Some((credit, taken_bytes)) = &mut self.credit
        {
            *taken_bytes += frame.charge();
            credit.grant(*taken_bytes + WINDOW);
        }
        async move {
            let (frame, set, mask) = self.session.take().await?;
            let frame = self.frame.insert(frame);
            Ok(Received {
                view: View::new(frame, mask),
                set,
            })
        }
    }
}

/// The key of each of `channels`, and their one index.
fn resolve(
    state: &State,
    channels: &[Name],
) -> Result<(Vec<channel::Key>, channel::Key), Error> {
    let mut keys = Vec::with_capacity(channels.len() + 1);
    let mut index = None;
    for name in channels {
        let channel = state
            .channels
            .get(name)
            .ok_or_else(|| Error::Unknown(name.clone()))?;
        if *index.get_or_insert(channel.index()) != channel.index() {
            return Err(Error::ManyIndexes);
        }
        keys.push(channel.key());
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
    /// Frames given in a row since `take` last returned `Pending`.
    streak: u32,
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
            streak: 0,
        }
    }

    /// The next frame, its key set, and the mask of the reader's channels in it. After
    /// [`STREAK`] frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// As [`Reader::next`].
    ///
    /// # Panics
    ///
    /// When the interner does not hold the key set of the frame, which a writer of
    /// this hub made.
    pub(crate) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Ended> {
        let frame = poll_fn(|cx| {
            if let Some(key) = self.removed.get() {
                return Poll::Ready(Err(Ended::Removed(key)));
            }
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
                Next::Behind => return Poll::Ready(Err(Ended::Behind)),
                Next::Empty => {}
            }
            if let Some(error) = &state.failed {
                return Poll::Ready(Err(Ended::Buffer(error.clone())));
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
