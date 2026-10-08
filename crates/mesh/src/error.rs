use std::fmt;
use std::path::PathBuf;

use env::files;

use raft::Position;
use types::ed25519::PublicKey;
use types::node;

use crate::change::Unknown;
use crate::pointer::Pointer;
use crate::region::Unfit;
use crate::{claim, log};

/// Why a mesh call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
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
    /// A request from a member that is not a voter of this node's configuration, and
    /// that no committed configuration removed.
    NotVoter {
        /// The sender.
        from: node::Key,
    },
    /// A request from `from`, which a committed configuration removed: an earlier
    /// committed configuration held it, and the last one lacks it. The stream stops
    /// with the `removed` code, and `from` stops its group when this node is a voter
    /// of its configuration.
    Removed {
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
    /// This node is not a voter of its configuration, and only a voter proposes a
    /// change. Call it on a voter.
    NoVote,
    /// The region cannot hold a member record of the config.
    Member(Unfit),
    /// This node's private key is not the key of its member.
    WrongKey,
    /// The pool has no block now (`Exhausted` or `Refused`). Try again later. For the
    /// write of the log, the group takes no proposal and no message until the write
    /// ends. For the answer to a forwarded proposal, the peer gets no answer, and the
    /// group can hold the entry of the proposal.
    Pool(block::Error),
    /// A message on a stream is the byte form of no message, or is not one that its
    /// stream carries. The stream stopped with code 2, but after the answer only the
    /// half that `serve` reads stopped.
    Malformed,
    /// A stream of a peer, or its session, failed.
    Stream(transport::Error),
    /// The group stopped.
    Stopped(Stopped),
    /// The spec pointer is not the base of a spec change, so another change applied
    /// first. Read the spec again and apply on top of it.
    Stale {
        /// The base of the change.
        base: Pointer,
        /// The pointer when the change applied.
        pointer: Pointer,
    },
    /// A spec change lists more chunks than one change can list. Apply the change in
    /// smaller steps.
    Large {
        /// The count of chunks that the change lists.
        chunks: usize,
        /// The most chunks that one change lists.
        most: usize,
    },
    /// A spec has problems, in the order that [`spec::region::check`] gives them. Fix
    /// each problem as [`spec::region::Problem::fix`] says.
    Problems(Vec<spec::region::Problem>),
    /// The voters that hold the chunks of a spec change are not a majority of one
    /// half of the voters, so the pointer did not move. This half comes first of the
    /// halves that lack a majority, incoming before outgoing.
    Quorum {
        /// The voters of the half that hold the chunks.
        held: usize,
        /// The voters of the half.
        voters: usize,
    },
    /// A call of this node's chunk store failed.
    Blob(blob::Error),
    /// A file call on the file that names the spec in use failed.
    Files(files::Error),
    /// A file in the directory of the spec in use does not name a pointer.
    Stray {
        /// The file.
        path: PathBuf,
    },
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
            Self::Removed { from } => write!(
                f,
                "node {from} sent a request, but a committed configuration removed it"
            ),
            Self::PeerNotVoter { peer } => write!(
                f,
                "the peer with the public key {peer} forwarded a change, but no voter \
                 holds that key"
            ),
            Self::Claim(error) => error.fmt(f),
            Self::NotMember(key) => {
                write!(f, "node {key} is not a member of the region")
            }
            Self::NoVote => f.write_str(
                "this node is not a voter, and only a voter proposes a change",
            ),
            Self::Member(refused) => refused.fmt(f),
            Self::WrongKey => {
                f.write_str("the private key of this node is not the key of its member")
            }
            Self::Pool(cause) => {
                write!(f, "the pool has no block for the mesh now: {cause}")
            }
            Self::Malformed => f.write_str("a message on a mesh stream is not valid"),
            Self::Stream(cause) => write!(f, "a mesh stream failed: {cause}"),
            Self::Stopped(stopped) => write!(f, "the group stopped: {stopped}"),
            Self::Stale { base, pointer } => write!(
                f,
                "the spec changed: the pointer is {pointer}, not the base {base}"
            ),
            Self::Large { chunks, most } => write!(
                f,
                "the change lists {chunks} chunks, more than the {most} that one \
                 change can list"
            ),
            Self::Problems(problems) => {
                f.write_str("the spec has problems")?;
                let mut separator = ": ";
                for problem in problems {
                    write!(f, "{separator}{problem}")?;
                    separator = "; ";
                }
                Ok(())
            }
            Self::Quorum { held, voters } => write!(
                f,
                "{held} of {voters} voters hold the chunks of the spec change, not a \
                 majority"
            ),
            Self::Blob(error) => write!(f, "the chunk store failed: {error}"),
            Self::Files(error) => error.fmt(f),
            Self::Stray { path } => write!(
                f,
                "{} is in the directory of the spec in use, but it does not name a \
                 pointer",
                path.display()
            ),
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

impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        Self::Stream(error)
    }
}

/// Why a group stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
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
    /// Voter `by` answered `removed`: a committed configuration lacks this node. Open
    /// the mesh again only after a join that gives a new configuration.
    Removed {
        /// The voter that answered.
        by: node::Key,
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
            Self::Removed { by } => write!(
                f,
                "voter {by} answered removed: a committed configuration lacks this node"
            ),
            Self::Dropped => f.write_str("each mesh of the group dropped"),
        }
    }
}

impl std::error::Error for Stopped {}
