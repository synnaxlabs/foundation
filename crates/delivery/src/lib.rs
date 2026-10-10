//! Keeps each reader's state per index: positions, holds, floors, position records,
//! credits, live frames for complete readers, latest mailbox, masks.

#![deny(clippy::wildcard_enum_match_arm)]

pub mod named;
mod readers;

use std::fmt;

use types::time::{Span, Stamp};

pub use readers::{Key, Next, Readers, complete, latest};

/// A reader's position on one index: on each path, the seq of the first sample it has
/// not received. It has every sample below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The first live sample the reader has not received.
    pub live: u64,
    /// The first backfill sample the reader has not received, or `None` when the reader
    /// does not record.
    pub backfill: Option<u64>,
}

impl fmt::Display for Position {
    /// Writes `live 10, backfill 4`, or `live 10` for a reader that does not record.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "live {}", self.live)?;
        match self.backfill {
            Some(backfill) => write!(f, ", backfill {backfill}"),
            None => Ok(()),
        }
    }
}

/// Who a session reads for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reader {
    /// A reader without a name. It holds data only while its session is open.
    Unnamed,
    /// A reader with a name, which belongs to the subject that opens it. It has at most
    /// one session at a time. After the session closes, the reader keeps its position
    /// and holds its data for `hold`. A hold of zero ends at the close.
    Named {
        /// The reader's subject and name.
        reader: named::Key,
        /// How long the reader holds its data after its session closes: zero or more.
        hold: Span,
    },
}

/// Where a new session starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// At `position`: the reader's `from` (now, oldest, seq, or time), resolved by the
    /// home.
    At(Position),
    /// Where the reader stopped: on each path, the position the reader's hub presents,
    /// else the named reader's position at this home, else `otherwise`. The reader
    /// records when `otherwise` has a backfill position.
    Resume {
        /// The position the reader's hub presents.
        presented: Option<Position>,
        /// The position when neither the hub nor this home has one.
        otherwise: Position,
    },
}

/// A named reader's state, for the index log. The last record of a reader, by its
/// key, replaces the ones before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The reader's subject and name.
    pub reader: named::Key,
    /// The reader's position.
    pub position: Position,
    /// How long the reader holds its data after its session closes.
    pub hold: Span,
    /// When the session closed, or `None` while it is open.
    pub closed: Option<Stamp>,
}

/// An input from a reader that breaks the delivery rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// An acknowledged position dropped a path or added one.
    Ack {
        /// The session's position.
        from: Position,
        /// The acknowledged position.
        to: Position,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ack { from, to } => write!(
                f,
                "acknowledged position must keep the reader's paths: from {from} to \
                 {to}"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_shows_each_path() {
        let live = Position {
            live: 10,
            backfill: None,
        };
        let recording = Position {
            live: 10,
            backfill: Some(4),
        };
        assert_eq!(live.to_string(), "live 10");
        assert_eq!(recording.to_string(), "live 10, backfill 4");
    }

    #[test]
    fn ack_error_names_both_positions() {
        let error = Error::Ack {
            from: Position {
                live: 10,
                backfill: Some(4),
            },
            to: Position {
                live: 9,
                backfill: Some(4),
            },
        };
        assert_eq!(
            error.to_string(),
            "acknowledged position must keep the reader's paths: from live 10, backfill \
             4 to live 9, backfill 4"
        );
    }
}
