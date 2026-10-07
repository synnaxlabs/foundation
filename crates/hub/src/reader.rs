//! Reader sessions: which frames one gets, why one does not open, and the session.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use types::frame::Frame;
use types::frame::key_set::KeySet;
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
    /// gets no later frame, with no error, until it closes: it misses one when it
    /// leaves a window of frames untaken, or when one commit holds more than a window
    /// of frames. The hub has no catch-up from the buffer yet.
    Complete,
    /// The newest live frame, before its commit.
    Latest,
}

/// One frame that a reader got.
#[derive(Debug)]
pub struct Received<'a> {
    /// The frame. Its series are encoded.
    pub frame: Frame,
    /// The key set that the frame's entries index.
    pub set: &'a Arc<KeySet>,
}

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
    /// The key set of the last frame.
    set: Option<Arc<KeySet>>,
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
        for name in channels {
            let channel = borrowed
                .channels
                .get(name)
                .ok_or_else(|| Error::Unknown(name.clone()))?;
            if *index.get_or_insert(channel.index) != channel.index {
                return Err(Error::ManyIndexes);
            }
        }
        let index = index.ok_or(Error::Empty)?;
        let slot = borrowed.interner.slots().assign(index);
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
            set: None,
            streak: 0,
        })
    }

    /// The next frame. After 128 frames in a row, it wakes its task and waits once, so
    /// a task that loops on it lets the shard's other tasks run. A complete session
    /// that missed a frame waits with no end ([`Mode::Complete`]).
    ///
    /// # Errors
    ///
    /// The error that ended the shard's buffer, once no frame waits, on this and every
    /// later call.
    #[expect(
        clippy::missing_panics_doc,
        reason = "the interner holds the key set of each frame a writer made"
    )]
    pub async fn next(&mut self) -> Result<Received<'_>, env::files::Error> {
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
            if let Some(error) = &state.failed {
                return Poll::Ready(Err(error.clone()));
            }
            self.streak = 0;
            state.wakers.insert(self.key, cx.waker().clone());
            Poll::Pending
        })
        .await?;
        let key = frame.key_set();
        let set = match self.set.take() {
            Some(set) if set.key() == key => self.set.insert(set),
            _ => {
                let snapshot = self.state.borrow().interner.snapshot();
                let set = snapshot
                    .get(key)
                    .expect("invariant: a frame's key set is known");
                self.set.insert(Arc::clone(set))
            }
        };
        Ok(Received { frame, set })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.wakers.remove(&self.key);
        state.home.close_reader(self.key);
    }
}
