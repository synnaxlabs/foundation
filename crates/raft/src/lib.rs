//! Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing
//! about specs.
//!
//! A [`Raft`] is the state machine of one node in one voter group. It does no I/O and
//! reads no clock. The caller gives it ticks and incoming messages. After each input,
//! the caller writes [`Raft::hard`] to disk if it changed, and only then sends
//! [`Raft::messages`].

mod config;
mod machine;
mod message;
#[cfg(test)]
mod safety;

use std::fmt;

use types::node;

pub use config::Config;
pub use machine::{Raft, Role};
pub use message::{Body, Message};

/// An election term. A term has at most one leader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Term(pub u64);

impl Term {
    fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("invariant: a term is below u64::MAX"),
        )
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The place of one log entry. Positions order by term first, then by index, so the
/// greater of two last positions is the more complete log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// The term in which a leader created the entry.
    pub term: Term,
    /// The entry's index in the log. The first entry has index 1.
    pub index: u64,
}

/// The state a node must have on disk before it sends a message, and must give back
/// to [`Raft::new`] after a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hard {
    /// The highest term the node has seen.
    pub term: Term,
    /// The node that has this node's vote in `term`.
    pub vote: Option<node::Key>,
}

/// Why a [`Raft`] rejected an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The tick counts cannot produce an election timeout.
    Ticks {
        /// The configured election ticks.
        election: u32,
        /// The configured heartbeat ticks.
        heartbeat: u32,
    },
    /// The voter list names a node twice.
    DuplicateVoter(node::Key),
    /// The stored term is lower than the term of the last log entry.
    TermBehindLog {
        /// The stored term.
        term: Term,
        /// The last log position.
        last: Position,
    },
    /// A message did not come from another node, or is for another node.
    Misrouted {
        /// The message's sender.
        from: node::Key,
        /// The message's receiver.
        to: node::Key,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ticks {
                election,
                heartbeat,
            } => write!(
                f,
                "election_ticks ({election}) must be greater than heartbeat_ticks \
                 ({heartbeat}), and heartbeat_ticks must be at least 1"
            ),
            Self::DuplicateVoter(key) => {
                write!(f, "node {:032x} is in the voter list twice", key.as_u128())
            }
            Self::TermBehindLog { term, last } => write!(
                f,
                "stored term {term} is lower than term {} of the last log entry",
                last.term
            ),
            Self::Misrouted { from, to } => write!(
                f,
                "a message from node {:032x} to node {:032x} is not for this node",
                from.as_u128(),
                to.as_u128()
            ),
        }
    }
}

impl std::error::Error for Error {}
