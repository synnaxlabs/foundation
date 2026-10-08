//! The keys and open results of complete sessions.

use std::fmt;

use types::channel::Slot;
use types::frame::key_set::KeySet;
use types::frame::{self, Frame};

use crate::Position;

/// A complete session on one index. Keys are unique within one
/// [`Readers`](crate::Readers). Use a key only with the `Readers` that gave it:
/// another one, such as a restored one, takes the key as its own when it gave the
/// same number, and panics when it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub(super) u64);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A complete session that [`Readers::open`](crate::Readers::open) started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The session.
    pub key: Key,
    /// Where the session starts.
    pub position: Position,
    /// The session of the same named reader that this one took over, in either mode.
    /// It is closed.
    pub replaced: Option<super::Key>,
}

/// What a frame costs the credit of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Charge {
    /// The frame's [`Frame::charge`]: the session gets the home's frame.
    Whole,
    /// The charge of a frame of one series for each slot, in this order, that the
    /// frame holds: the frame that a remote reader builds, as
    /// [`types::frame::Places::charge`] gives it. A slot after its first listing, or
    /// one the frame's key set lacks, adds no series.
    Places(Box<[Slot]>),
}

/// What frames cost one session, as its [`Charge`] says.
#[derive(Debug)]
pub(super) enum Cost {
    Whole,
    /// Boxed, so a session of the common case stays small.
    Places(Box<frame::Places>),
}

const _: () = assert!(
    size_of::<Cost>() == size_of::<usize>(),
    "a `Cost` grows each session by more than one pointer"
);

impl Cost {
    pub(super) fn new(charge: Charge) -> Self {
        match charge {
            Charge::Whole => Self::Whole,
            Charge::Places(slots) => Self::Places(Box::new(frame::Places::new(slots))),
        }
    }

    /// What `frame`, of key set `set` and [`Frame::charge`] `whole`, costs the
    /// session, in the time of [`frame::Places::charge`].
    pub(super) fn charge(&mut self, frame: &Frame, set: &KeySet, whole: u64) -> u64 {
        match self {
            Self::Whole => whole,
            Self::Places(places) => places.charge(frame, set),
        }
    }
}
