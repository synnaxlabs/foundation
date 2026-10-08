- **RAFT DURABILITY (#352)** `raft` is safe only when a disk keeps what it synced. The
  disk owns that (`env::files`); `mesh` writes each `Ready` there. `raft` does not
  find a loss. When a follower's disk lost synced entries, and what it applied of
  them, the leader still counts them. While the leader's commit is below the
  follower's last entry, the follower follows, and the leader can commit an entry
  that fewer than a quorum hold. Once the commit passes that entry, each heartbeat
  gives the follower `Error::IndexPastLog`; an append to it fails with no error. A
  loss that keeps `applied` fails at `Raft::new` with `Error::AppliedPastLog`. `node`
  shows the error in its status (#648). Lost: the leader sends again from below what
  it counted, which lowers its count under a commit that a quorum may no longer
  hold. The person left the choice to the coordinator on 2026-10-05 ("your choice",
  #352 item 3), and the coordinator chose this. Later, at low priority: the leader
  learns the follower's real last index and stops counting lost entries (#663).
