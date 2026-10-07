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
    /// A request from a node that is not a voter of this node's configuration.
    NotVoter,
    /// A message carries a grant that does not hold.
    Grant(grant::Error),
    /// A call names a node that is not a member of the region.
    NotMember(node::Key),
    /// Two member records name one node.
    Duplicate(node::Key),
    /// This node's private key is not the key of its member.
    WrongKey,
    /// The group stopped.
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
            Self::NotVoter => {
                f.write_str("a request came from a node that is not a voter")
            }
            Self::Grant(error) => error.fmt(f),
            Self::NotMember(key) => {
                write!(f, "node {key} is not a member of the region")
            }
            Self::Duplicate(key) => write!(f, "node {key} has two member records"),
            Self::WrongKey => {
                f.write_str("the private key of this node is not the key of its member")
            }
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
    /// A write of the log failed, so `raft` cannot go on. Open the mesh again.
    Write(log::Error),
    /// The committed entry at `at` is not a change that this build reads. A new open
    /// stops at the same entry.
    Change {
        /// The position of the entry.
        at: Position,
        /// Why its bytes are not a change.
        cause: Malformed,
    },
    /// Each `Mesh` of the group dropped. Only `Watch::next` gives it: get a new watch
    /// from the mesh that opens next.
    Dropped,
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
            Self::Dropped => f.write_str("each mesh of the group dropped"),
        }
    }
}
