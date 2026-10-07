use std::fmt;

use raft::Position;
use types::node::{self, PublicKey};

use crate::change::Unknown;
use crate::region::Unfit;
use crate::{claim, log, status};

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
    /// A forwarded change from a peer whose key no voter of this node's configuration
    /// holds.
    PeerNotVoter {
        /// The key that the peer proved.
        peer: PublicKey,
    },
    /// A message carries a claim that does not hold.
    Claim(claim::Error),
    /// A call names a node that is not a member of the region.
    NotMember(node::Key),
    /// The region cannot hold a member record of the config.
    Member(Unfit),
    /// This node's private key is not the key of its member.
    WrongKey,
    /// This node has no mesh time that can stamp a join: none yet, one with an unknown
    /// error, or one whose later edge is before the Unix epoch.
    Unsynced,
    /// A join request names more than 64 status channels.
    Status(status::Many),
    /// The pool has no block now (`Exhausted` or `Refused`). Try again later. For the
    /// write of the log, the group takes no proposal and no message until the write
    /// ends. For the answer to a forwarded proposal, the group did not see the
    /// proposal.
    Pool(block::Error),
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
            Self::NotVoter { from } => {
                write!(f, "node {from} sent a request, but it is not a voter")
            }
            Self::PeerNotVoter { peer } => write!(
                f,
                "the peer with the public key {peer} forwarded a change, but no voter \
                 holds that key"
            ),
            Self::Claim(error) => error.fmt(f),
            Self::NotMember(key) => {
                write!(f, "node {key} is not a member of the region")
            }
            Self::Member(refused) => refused.fmt(f),
            Self::WrongKey => {
                f.write_str("the private key of this node is not the key of its member")
            }
            Self::Unsynced => f.write_str(
                "this node has no mesh time with a known error at or after the Unix \
                 epoch, so it stamps no join",
            ),
            Self::Status(many) => many.fmt(f),
            Self::Pool(cause) => {
                write!(f, "the pool has no block for the mesh now: {cause}")
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

impl From<claim::Error> for Error {
    fn from(error: claim::Error) -> Self {
        Self::Claim(error)
    }
}

/// Why a group stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// A write of the log failed, so `raft` cannot go on. Open the mesh again.
    Write(log::Error),
    /// The committed change at `at` has 0 bytes or a kind that this build does not
    /// know. A new open stops at the same entry.
    Change {
        /// The position of the entry.
        at: Position,
        /// Why its bytes are not a change.
        cause: Unknown,
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
