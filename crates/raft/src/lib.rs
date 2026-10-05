//! Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing
//! about specs.
//!
//! A [`Raft`] is the state machine of one node in one voter group. It does no I/O and
//! reads no clock. The caller gives it ticks and incoming messages. After each input,
//! the caller takes a [`Ready`] and does what it says, in its order.

#![deny(clippy::wildcard_enum_match_arm)]

mod config;
mod log;
mod machine;
mod message;
mod progress;
mod voters;

use std::fmt;

use types::node;

pub use config::{Config, Start};
pub use log::Entry;
pub use machine::{Raft, Ready, Role};
pub use message::{Body, Message};
pub use voters::Voters;

/// An election term. A term has at most one leader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Term(pub u64);

impl Term {
    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
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
    /// One voter list names a node twice.
    DuplicateVoter(node::Key),
    /// The stored term is lower than the term of the last log entry.
    TermBehindLog {
        /// The stored term.
        term: Term,
        /// The last log position.
        last: Position,
    },
    /// A log entry does not follow the one before it: its index is not the next
    /// index, or its term is lower or zero.
    EntryOutOfOrder {
        /// The position of the entry.
        at: Position,
        /// The position of the entry before it, or zero for the first entry.
        before: Position,
    },
    /// The applied index is past the end of the log.
    AppliedPastLog {
        /// The applied index.
        applied: u64,
        /// The last log index.
        last: u64,
    },
    /// A proposal to a node that does not lead.
    NotLeader {
        /// The node that leads, when this node knows it.
        leader: Option<node::Key>,
    },
    /// A message is for another node. The caller routed it wrongly.
    Misrouted {
        /// The message's receiver.
        to: node::Key,
    },
    /// A message names this node as its sender.
    Loopback,
    /// A second node claims to lead a term that this node leads. Election safety is
    /// broken, or the sender is faulty.
    SecondLeader {
        /// The term with two leaders.
        term: Term,
        /// The other leader.
        from: node::Key,
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
            Self::EntryOutOfOrder { at, before } => write!(
                f,
                "log entry at index {} in term {} does not follow index {} in term {}",
                at.index, at.term, before.index, before.term
            ),
            Self::AppliedPastLog { applied, last } => write!(
                f,
                "applied index {applied} is past the last log index {last}"
            ),
            Self::NotLeader {
                leader: Some(leader),
            } => write!(
                f,
                "this node does not lead; node {:032x} does",
                leader.as_u128()
            ),
            Self::NotLeader { leader: None } => {
                f.write_str("this node does not lead, and knows no leader")
            }
            Self::Misrouted { to } => write!(
                f,
                "a message for node {:032x} is not for this node",
                to.as_u128()
            ),
            Self::Loopback => f.write_str("a message names this node as its sender"),
            Self::SecondLeader { term, from } => write!(
                f,
                "node {:032x} also claims to lead term {term}",
                from.as_u128()
            ),
        }
    }
}

impl std::error::Error for Error {}
