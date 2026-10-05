use types::node;

use crate::{Position, Term};

/// One message between two nodes of a voter group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    /// The sender.
    pub from: node::Key,
    /// The receiver.
    pub to: node::Key,
    /// The sender's term. A PreVote carries the term the sender would campaign in, and
    /// a reply that grants a PreVote or a vote carries the term of the request.
    pub term: Term,
    /// What the message says.
    pub body: Body,
}

/// What a [`Message`] says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
    /// Asks whether the receiver would vote for the sender. It changes no term.
    PreVote {
        /// The sender's last log position.
        last: Position,
    },
    /// Answers a [`Body::PreVote`].
    PreVoteReply {
        /// Whether the receiver would vote for the sender.
        granted: bool,
    },
    /// Asks for the receiver's vote in the message's term.
    Vote {
        /// The sender's last log position.
        last: Position,
    },
    /// Answers a [`Body::Vote`].
    VoteReply {
        /// Whether the sender has the vote.
        granted: bool,
    },
    /// A leader states that it leads the message's term.
    Heartbeat,
    /// Answers a [`Body::Heartbeat`]. A leader counts it as contact for CheckQuorum.
    HeartbeatReply,
}
