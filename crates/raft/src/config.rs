use types::node;

use crate::{Entry, Hard, Message, Voters};

/// The fixed inputs of a [`Raft`](crate::Raft).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// This node.
    pub key: node::Key,
    /// The ticks a follower waits without a leader before it starts an election. The
    /// real wait is random, from this value to one tick below twice this value.
    pub election_ticks: u32,
    /// The ticks between a leader's heartbeats to a voter. A learner gets one each
    /// `election_ticks` ([`Raft::set_learners`](crate::Raft::set_learners)). Must be
    /// at least 1 and lower than `election_ticks`.
    pub heartbeat_ticks: u32,
    /// Whether the caller can send `message` to each peer. An `Append` with no proof
    /// drops its last entries until it fits, down to one, so `fits` must hold an
    /// `Append` of the largest entry with no proof. A message with a proof drops its
    /// entries, then the oldest links of its chain, so `fits` must hold each message
    /// with no entry and no link. `raft` panics when `fits` is false for an `Append`
    /// of one entry with no proof, or for a message with no entry and no link.
    pub fits: fn(&Message) -> bool,
}

/// The state a [`Raft`](crate::Raft) starts from: what the node had on disk. A node
/// that founds a group, or that joins one, starts from `Start::default()` plus the
/// founding voters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Start {
    /// The stored term and vote.
    pub hard: Hard,
    /// The configuration before `entries`: the last `Voters` entry in them replaces
    /// it. Empty only for a voter that an operator wiped: it takes any proof until it
    /// holds a `Voters` entry, and the first one then shows the configuration before
    /// it, a joint entry's outgoing set or a leave's own set. A node that is not a
    /// voter never starts an election while its configuration is committed, but it
    /// still votes and follows.
    pub voters: Voters,
    /// The log on disk, from index 1, in order.
    pub entries: Vec<Entry>,
    /// The index of the last entry the caller applied. The committed entries of a
    /// [`Ready`](crate::Ready) start after it.
    pub applied: u64,
}
