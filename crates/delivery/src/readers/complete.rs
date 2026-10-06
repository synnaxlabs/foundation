//! The keys and open results of complete sessions.

use std::fmt;

use crate::Position;

/// A complete session on one index. Keys are unique within one
/// [`Readers`](crate::Readers).
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
