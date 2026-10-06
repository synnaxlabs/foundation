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
pub use log::{Data, Entry};
pub use machine::{Raft, Ready, Role};
pub use message::{Body, Grant, Message, Proof};
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hard {
    /// The highest term the node has seen.
    pub term: Term,
    /// The node that has this node's vote in `term`.
    pub vote: Option<node::Key>,
    /// The leader of `term` that this node heard, or this node when it led. It
    /// stays through a step-down until the term ends.
    pub leader: Option<node::Key>,
    /// The proof of `term`: this node's pre-votes when it campaigned, else the proof
    /// of the message that moved it, else its leader's votes. `None` at term zero,
    /// and until one arrives. A node with no proof answers no message of a lower
    /// term.
    pub proof: Option<Proof>,
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
    /// The incoming voter set of `Start.voters` is empty while the outgoing set is
    /// not.
    EmptyIncoming,
    /// A configuration has an empty incoming voter set.
    NoVoters,
    /// A configuration change is in progress: the configuration entry at `at` is not
    /// committed yet.
    ChangePending {
        /// The position of the last configuration entry in the log.
        at: Position,
    },
    /// An append's term is lower than the term of an entry it carries.
    TermBehindLog {
        /// The covering term.
        term: Term,
        /// The position of the last entry it must cover.
        last: Position,
    },
    /// A heartbeat, an append reply, or an append reject names a log index past this
    /// node's last entry. A heartbeat commits only what the follower holds, and a
    /// follower answers only for entries the leader sent. So the sender is faulty, or,
    /// from a heartbeat, this node's disk lost entries it synced.
    IndexPastLog {
        /// The index the message names.
        index: u64,
        /// The last log index.
        last: u64,
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
    /// A heartbeat or an append of this node's term from a node other than the
    /// leader of the term it knows: the one it heard, or itself. Election safety is
    /// broken, or the sender is faulty.
    SecondLeader {
        /// The term.
        term: Term,
        /// The sender.
        from: node::Key,
    },
    /// A message that claims a term this node is not in, or a leader of its term
    /// while this node knows none, with no proof that a quorum of this node's
    /// configuration, in force or last committed, granted it. The sender is faulty,
    /// or it holds a configuration this node lacks.
    Unproven {
        /// The term the message claims.
        term: Term,
        /// The sender.
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
            Self::EmptyIncoming => {
                write!(
                    f,
                    "the incoming voter set is empty while the outgoing set is not"
                )
            }
            Self::NoVoters => {
                write!(f, "a configuration has an empty incoming voter set")
            }
            Self::ChangePending { at } => write!(
                f,
                "a configuration change at index {} in term {} is pending",
                at.index, at.term
            ),
            Self::TermBehindLog { term, last } => write!(
                f,
                "term {term} is lower than term {} of entry {}",
                last.term, last.index
            ),
            Self::IndexPastLog { index, last } => {
                write!(f, "index {index} is past the last log index {last}")
            }
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
            Self::Unproven { term, from } => write!(
                f,
                "node {:032x} claims term {term} with no proof this node accepts",
                from.as_u128()
            ),
        }
    }
}

impl std::error::Error for Error {}
