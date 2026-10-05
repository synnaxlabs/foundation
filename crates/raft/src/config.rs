use types::node;

/// The fixed inputs of a [`Raft`](crate::Raft).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// This node.
    pub key: node::Key,
    /// The nodes whose votes count. A node that is not in its own list never starts
    /// an election, but it still votes and follows.
    pub voters: Vec<node::Key>,
    /// The ticks a follower waits without a leader before it starts an election. The
    /// real wait is random, from this value to one tick below twice this value.
    pub election_ticks: u32,
    /// The ticks between a leader's heartbeats. Must be at least 1 and lower than
    /// `election_ticks`.
    pub heartbeat_ticks: u32,
}
