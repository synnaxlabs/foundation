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
    pub(crate) fn open(
        state: &Rc<RefCell<State>>,
        channels: &[Name],
        mode: Mode,
    ) -> Result<Self, Error> {
        let mut slots = Vec::with_capacity(channels.len());
        let slot = {
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
        slots.push(slot);
        let slots = slots.into();
        let (session, credit) = match mode {
            Mode::Complete => {
                let (session, credit) = Session::complete(state, slots, slot, WINDOW);
                (session, Some((credit, 0)))
            }
            Mode::Latest => (Session::latest(state, slots, slot), None),
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
    /// [`Ended`] once no frame waits and the session can give no more, on this and
    /// every later call: [`Ended::Behind`] before [`Ended::Buffer`].
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

/// A session at the shard's home, through a mask of the reader's channels: the frames
/// it takes, and why it ends. Each [`Reader`] drives one, and so does each stream of a
/// remote reader.
#[derive(Debug)]
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// The key of a complete session.
    complete: Option<::home::reader::complete::Key>,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The key set of the last frame, and the mask of the reader's channels in it.
    mask: Option<(Arc<KeySet>, Mask)>,
    /// Frames given in a row since `take` last returned `Pending`.
    streak: u32,
}

impl Session {
    /// Opens a complete session through `slots` on the index of `index`, with a grant
    /// of `limit_bytes`. Returns the session and the credit that raises its grant.
    pub(crate) fn complete(
        state: &Rc<RefCell<State>>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
        limit_bytes: u64,
    ) -> (Self, Credit) {
        let key = state.borrow_mut().home.open_complete(
            index,
            limit_bytes,
            home::reader::complete::Charge::Whole,
        );
        let credit = Credit {
            state: Rc::clone(state),
            key,
        };
        (Self::new(state, key.into(), Some(key), slots), credit)
    }

    /// Opens a latest session through `slots` on the index of `index`.
    pub(crate) fn latest(
        state: &Rc<RefCell<State>>,
        slots: Box<[channel::Slot]>,
        index: channel::Slot,
    ) -> Self {
        let key = state.borrow_mut().home.open_latest(index);
        Self::new(state, key, None, slots)
    }

    fn new(
        state: &Rc<RefCell<State>>,
        key: ::home::reader::Key,
        complete: Option<::home::reader::complete::Key>,
        slots: Box<[channel::Slot]>,
    ) -> Self {
        Self {
            state: Rc::clone(state),
            key,
            complete,
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
    /// [`Ended`] once no frame waits and the session can give no more, on this and
    /// every later call: [`Ended::Behind`] before [`Ended::Buffer`].
    ///
    /// # Panics
    ///
    /// When the interner does not hold the key set of the frame, which a writer of
    /// this hub made.
    pub(crate) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Ended> {
        let frame = poll_fn(|cx| {
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
