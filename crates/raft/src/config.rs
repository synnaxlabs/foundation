use types::node;

use crate::{Entry, Hard, Voters};

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

/// The state a [`Raft`](crate::Raft) starts from: what the node had on disk. A node
/// that founds a group starts from `Start::default()` plus the founding voters, and
/// a node that joins one from `Start::default()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Start {
    /// The stored term and vote.
    pub hard: Hard,
    /// The configuration before `entries`: the last `Voters` entry in them replaces
    /// it. When it is empty, the first `Voters` entry shows it instead: a joint
    /// entry's outgoing set, or a leave's own set. A node that is not a voter never
    /// starts an election while its configuration is committed, but it still votes
    /// and follows.
    pub voters: Voters,
    /// The log on disk, from index 1, in order.
    pub entries: Vec<Entry>,
    /// The index of the last entry the caller applied. The committed entries of a
    /// [`Ready`](crate::Ready) start after it.
    pub applied: u64,
}
