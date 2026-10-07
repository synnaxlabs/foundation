use std::fmt;

use raft::Position;
use types::node;

use crate::region::Malformed;
use crate::{grant, log};

/// Why a mesh call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The log did not open.
    Log(log::Error),
    /// `raft` refused the log, a message, or a proposal.
    Raft(raft::Error),
    /// A message names `from` as its sender, but the peer that sent it does not hold
    /// the key of that member.
    Spoofed {
        /// The sender that the message names.
        from: node::Key,
    },
    /// A request from a member that is not a voter of this node's configuration.
    NotVoter {
        /// The sender.
        from: node::Key,
    },
    /// A message carries a grant that does not hold.
    Grant(grant::Error),
    /// The group stopped. Open the mesh again.
    Stopped(Stopped),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(error) => error.fmt(f),
            Self::Raft(error) => error.fmt(f),
            Self::Spoofed { from } => write!(
                f,
                "a message names node {from} as its sender, but its peer does not \
                 hold the key of that member"
            ),
            Self::NotVoter { from } => {
                write!(f, "node {from} sent a request, but it is not a voter")
            }
            Self::Grant(error) => error.fmt(f),
            Self::Stopped(stopped) => write!(f, "the group stopped: {stopped}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<log::Error> for Error {
    fn from(error: log::Error) -> Self {
        Self::Log(error)
    }
}

impl From<raft::Error> for Error {
    fn from(error: raft::Error) -> Self {
        Self::Raft(error)
    }
}

impl From<grant::Error> for Error {
    fn from(error: grant::Error) -> Self {
        Self::Grant(error)
    }
}

/// Why a group stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// A write of the log failed, so `raft` cannot go on.
    Write(log::Error),
    /// The committed entry at `at` is not a change that this build reads.
    Change {
        /// The position of the entry.
        at: Position,
        /// Why its bytes are not a change.
        cause: Malformed,
    },
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Write(error) => error.fmt(f),
            Self::Change { at, cause } => write!(
                f,
                "the committed entry at index {} of term {} is not a change: {cause}",
                at.index, at.term.0
            ),
        }
    }
}
