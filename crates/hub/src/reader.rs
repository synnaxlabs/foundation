//! Reader sessions: which frames one gets, why one does not open, and the session.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{Frame, Mask, View};
use types::name::Name;

use crate::State;

/// The credit a complete reader has past the frames it gave back: a fixed window until
/// the hub sizes it from the link.
const WINDOW: u64 = 1 << 20;
/// Frames that [`Reader::next`] gives in a row before it yields once.
const STREAK: u32 = 128;

/// Which frames a reader gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Each live frame, after the commit that holds it. A session that misses a frame
    /// ends with [`Ended::Behind`] after the frames before it: it misses one that
    /// comes when the frames it has not given back (the one it holds and those it has
    /// not taken) reach a window.
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
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(error) => write!(f, "the buffer of the shard failed: {error}"),
            Self::Behind => f.write_str(
                "the reader missed a frame and gets no later one: open a new reader",
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no channel is named {name}"),
            Self::ManyIndexes => f.write_str(
                "the channels are on more than one index: open a reader per index",
            ),
            Self::Empty => f.write_str("a reader names at least one channel"),
        }
    }
}

impl std::error::Error for Error {}

/// A reader session. Dropping it closes the session; frames that wait do not go out.
#[derive(Debug)]
pub struct Reader {
    session: Session,
    /// The charge of each frame that a complete reader took and gave back.
    taken_bytes: Option<u64>,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The frame that the last [`Received`] lends.
    frame: Option<Frame>,
    /// The key set of the last frame, and the mask of the reader's channels in it.
    mask: Option<(Arc<KeySet>, Mask)>,
}

impl Reader {
    /// Opens a reader on the index of `channels`.
    pub(crate) fn open(
        state: &Rc<RefCell<State>>,
        channels: &[Name],
        mode: Mode,
    ) -> Result<Self, Error> {
        let mut slots = Vec::with_capacity(channels.len());
        let index = {
            let mut borrowed = state.borrow_mut();
            let borrowed = &mut *borrowed;
            let mut index = None;
            for name in channels {
                let channel = borrowed
                    .channels
                    .get(name)
                    .ok_or_else(|| Error::Unknown(name.clone()))?;
                if *index.get_or_insert(channel.index) != channel.index {
                    return Err(Error::ManyIndexes);
                }
                slots.push(borrowed.interner.slots().assign(channel.key));
            }
            let index = index.ok_or(Error::Empty)?;
            borrowed.interner.slots().assign(index)
        };
        // A frame without the reader's channels still shows that time moved.
        slots.push(index);
        let (session, taken_bytes) = match mode {
            Mode::Complete => (Session::complete(state, index, WINDOW), Some(0)),
            Mode::Latest => (Session::latest(state, index), None),
        };
        Ok(Self {
            session,
            taken_bytes,
            slots: slots.into(),
            frame: None,
            mask: None,
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
        clippy::missing_panics_doc,
        reason = "the interner holds the key set of each frame a writer made"
    )]
    #[expect(
        clippy::should_implement_trait,
        reason = "it gives a future, which `Iterator::next` cannot"
    )]
    pub fn next(&mut self) -> impl Future<Output = Result<Received<'_>, Ended>> {
        if let Some(frame) = self.frame.take()
            && let Some(taken_bytes) = &mut self.taken_bytes
        {
            *taken_bytes += frame.charge();
            self.session.grant(*taken_bytes + WINDOW);
        }
        async move {
            let frame = self.session.take().await?;
            let key = frame.key_set();
            let (set, mask) = match self.mask.take() {
                Some(mask) if mask.0.key() == key => self.mask.insert(mask),
                _ => {
                    let snapshot = self.session.state.borrow().interner.snapshot();
                    let set = snapshot
                        .get(key)
                        .expect("invariant: a frame's key set is known");
                    let mask = Mask::new(set, self.slots.iter().copied());
                    self.mask.insert((Arc::clone(set), mask))
                }
            };
            let frame = self.frame.insert(frame);
            Ok(Received {
                view: View::new(frame, mask),
                set,
            })
        }
    }
}

/// A reader session at the shard's home: the frames it takes, and why it ends. Each
/// [`Reader`] drives one, and so does each stream of a remote reader.
#[derive(Debug)]
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// The key of a complete session.
    complete: Option<::home::reader::complete::Key>,
    /// Frames given in a row since `take` last returned `Pending`.
    streak: u32,
}

impl Session {
    /// Opens a complete session on the index of `slot`, with a grant of
    /// `limit_bytes`.
    pub(crate) fn complete(
        state: &Rc<RefCell<State>>,
        slot: channel::Slot,
        limit_bytes: u64,
    ) -> Self {
        let complete = state.borrow_mut().home.open_complete(slot, limit_bytes);
        Self::new(state, complete.into(), Some(complete))
    }

    /// Opens a latest session on the index of `slot`.
    pub(crate) fn latest(state: &Rc<RefCell<State>>, slot: channel::Slot) -> Self {
        let key = state.borrow_mut().home.open_latest(slot);
        Self::new(state, key, None)
    }

    fn new(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::Key,
        complete: Option<::home::reader::complete::Key>,
    ) -> Self {
        Self {
            state: Rc::clone(state),
            key,
            complete,
            streak: 0,
        }
    }

    /// Raises the grant of a complete session to `limit_bytes` since the open.
    ///
    /// # Panics
    ///
    /// When the session is latest.
    pub(crate) fn grant(&self, limit_bytes: u64) {
        let complete = self.complete.expect("only a complete session has credit");
        self.state.borrow_mut().home.grant(complete, limit_bytes);
    }

    /// The next frame. After [`STREAK`] frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// As [`Reader::next`].
    pub(crate) async fn take(&mut self) -> Result<Frame, Ended> {
        poll_fn(|cx| {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            if self.streak == STREAK {
                self.streak = 0;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if let Some(frame) = state.home.take(self.key) {
                self.streak += 1;
                return Poll::Ready(Ok(frame));
            }
            if let Some(complete) = self.complete
                && state.home.behind(complete)
            {
                return Poll::Ready(Err(Ended::Behind));
            }
            if let Some(error) = &state.failed {
                return Poll::Ready(Err(Ended::Buffer(error.clone())));
            }
            self.streak = 0;
            state.wakers.insert(self.key, cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.wakers.remove(&self.key);
        state.home.close_reader(self.key);
    }
}
