use types::node;

use crate::{Entry, Position, Term};

/// One message between two nodes of a voter group.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    Heartbeat {
        /// The leader's commit index, no higher than what the receiver holds.
        commit: u64,
    },
    /// Answers a [`Body::Heartbeat`]. A leader counts it as contact for CheckQuorum.
    HeartbeatReply,
    /// Entries after `prev`, from the leader. An empty list probes the receiver's
    /// log or carries a new commit index.
    Append {
        /// The position the entries follow. The receiver takes them only when it
        /// holds it.
        prev: Position,
        /// The entries, in order from `prev.index + 1`.
        entries: Vec<Entry>,
        /// The leader's commit index.
        commit: u64,
    },
    /// Answers a [`Body::Append`] whose entries the receiver took.
    AppendReply {
        /// The index of the last entry the leader sent, which the receiver holds.
        last: u64,
    },
    /// Answers a [`Body::Append`] whose `prev` the receiver did not hold.
    AppendReject {
        /// The receiver's hint for the next `prev`: the last index its log may
        /// share with the leader's.
        hint: u64,
    },
}
