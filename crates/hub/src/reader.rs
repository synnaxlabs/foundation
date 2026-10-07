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

/// The credit a complete reader has past the frames it took: a fixed window until
/// the hub sizes it from the link.
const WINDOW: u64 = 1 << 20;
/// Frames that [`Reader::next`] gives in a row before it yields once.
const STREAK: u32 = 128;

/// Which frames a reader gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Each live frame, after the commit that holds it. A session that misses a frame
    /// ends with [`Ended::Behind`] after the frames before it: it misses one when it
    /// leaves a window of frames untaken, or when one commit holds more than a window
    /// of frames. The hub has no catch-up from the buffer yet.
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
    state: Rc<RefCell<State>>,
    key: ::home::reader::Key,
    /// The credit of a complete reader.
    credit: Option<Credit>,
    /// The slots of the reader's channels.
    slots: Box<[channel::Slot]>,
    /// The frame that the last [`Received`] lends.
    frame: Option<Frame>,
    /// The key set of the last frame, and the mask of the reader's channels in it.
    mask: Option<(Arc<KeySet>, Mask)>,
    /// Frames given in a row since `next` last returned `Pending`.
    streak: u32,
}

/// What a complete reader took, to grant its credit.
#[derive(Debug)]
struct Credit {
    key: ::home::reader::complete::Key,
    /// The charge of each frame taken.
    taken_bytes: u64,
}

impl Reader {
    /// Opens a reader on the index of `channels`.
    pub(crate) fn open(
        state: &Rc<RefCell<State>>,
        channels: &[Name],
        mode: Mode,
    ) -> Result<Self, Error> {
        let mut borrowed = state.borrow_mut();
        let borrowed = &mut *borrowed;
        let mut index = None;
        let mut slots = Vec::with_capacity(channels.len());
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
        let slot = borrowed.interner.slots().assign(index);
        // A frame without the reader's channels still shows that time moved.
        slots.push(slot);
        let (key, credit) = match mode {
            Mode::Complete => {
                let key = borrowed.home.open_complete(slot, WINDOW);
                let credit = Credit {
                    key,
                    taken_bytes: 0,
                };
                (key.into(), Some(credit))
            }
            Mode::Latest => (borrowed.home.open_latest(slot), None),
        };
        Ok(Self {
            state: Rc::clone(state),
            key,
            credit,
            slots: slots.into(),
            frame: None,
            mask: None,
            streak: 0,
        })
    }

    /// The next frame, as a view of the reader's channels. The view borrows the
    /// reader, so the frame stays in use until the next call or the drop. After 128
    /// frames in a row, it wakes its task and waits once, so a task that loops on it
    /// lets the shard's other tasks run.
    ///
    /// # Errors
    ///
    /// [`Ended`] once no frame waits and the session can give no more, on this and
    /// every later call: [`Ended::Behind`] before [`Ended::Buffer`].
    #[expect(
        clippy::missing_panics_doc,
        reason = "the interner holds the key set of each frame a writer made"
    )]
    pub async fn next(&mut self) -> Result<Received<'_>, Ended> {
        self.frame = None;
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
                if let Some(credit) = &mut self.credit {
                    // The limit rises only for frames the caller took before.
                    state.home.grant(credit.key, credit.taken_bytes + WINDOW);
                    credit.taken_bytes += frame.charge();
                }
                return Poll::Ready(Ok(frame));
            }
            if let Some(credit) = &self.credit
                && state.home.behind(credit.key)
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
        let frame = self.frame.insert(frame);
        Ok(Received {
            view: View::new(frame, mask),
            set,
        })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.wakers.remove(&self.key);
        state.home.close_reader(self.key);
    }
}
