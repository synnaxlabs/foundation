use types::node;

use crate::{Entry, Hard};

/// The fixed inputs of a [`Raft`](crate::Raft).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// This node.
    pub key: node::Key,
    /// The ticks a follower waits without a leader before it starts an election. The
    /// real wait is random, from this value to one tick below twice this value.
    pub election_ticks: u32,
    /// The ticks between a leader's heartbeats. Must be at least 1 and lower than
    /// `election_ticks`.
    pub heartbeat_ticks: u32,
}

/// The state a [`Raft`](crate::Raft) starts from: what the node had on disk. A new
/// node starts from `Start::default()` plus its voters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Start {
    /// The stored term and vote.
    pub hard: Hard,
    /// The nodes whose votes count. A node that is not in its own list never starts
    /// an election, but it still votes and follows.
    pub voters: Vec<node::Key>,
    /// The log on disk, from index 1, in order.
    pub entries: Vec<Entry>,
    /// The index of the last entry the caller applied. The committed entries of a
    /// [`Ready`](crate::Ready) start after it.
    pub applied: u64,
}
