# Raft conformance

Election and replication scenarios ported from the tests of etcd/raft
(<https://github.com/etcd-io/raft>, commit `1c0011d2c6b7`, Copyright 2015 The etcd
Authors, Apache License 2.0). `crates/raft` runs them as its `conformance` test. The
`[[test]]` entry in `crates/raft/Cargo.toml` is part of this oracle: to remove it is
to weaken the oracle.

`LICENSE` is the license of the etcd source. `election.rs` and `replication.rs` are
modified works: ports from Go to Rust, and the port rules below list the changes.

```sh
cargo test -p raft --test conformance
```

## Port rules

- Each scenario keeps the name and the steps of its etcd source.
- Foundation's Raft always runs PreVote and CheckQuorum. etcd runs some scenarios with
  one of them off. Such a scenario is adapted, and its comment states the difference.
- A scenario reaches a state only through the public surface of `raft`: a node
  starts from its stored state and log, and an election runs through messages where
  etcd calls `becomeLeader`. A `Disk` does what each `Ready` says, so a scenario reads
  the log and the applied entries that etcd reads from `raftLog`.
- A message that etcd hands to a handler directly carries the term the handler
  expects.

## Scenarios

| Scenario | etcd source |
| --- | --- |
| `leader_election` | `TestLeaderElectionPreVote` |
| `single_node` | `TestSingleNodePreCandidate` |
| `vote_from_any_state` | `TestVoteFromAnyState` |
| `prevote_from_any_state` | `TestPreVoteFromAnyState` |
| `recv_vote`, `recv_prevote` | `TestRecvMsgVote`, `TestRecvMsgPreVote` (follower cases) |
| `dueling_pre_candidates` | `TestDuelingPreCandidates` |
| `node_with_smaller_term_can_complete_election` | `TestNodeWithSmallerTermCanCompleteElection` |
| `prevote_with_split_vote` | `TestPreVoteWithSplitVote` |
| `prevote_with_check_quorum` | `TestPreVoteWithCheckQuorum` |
| `prevote_checkquorum` | `testdata/prevote_checkquorum.txt` |
| `leader_stepdown_when_quorum_active` | `TestLeaderStepdownWhenQuorumActive` |
| `leader_stepdown_when_quorum_lost` | `TestLeaderStepdownWhenQuorumLost` |
| `leader_superseding_with_check_quorum` | `TestLeaderSupersedingWithCheckQuorum` |
| `free_stuck_candidate_with_check_quorum` | `TestFreeStuckCandidateWithCheckQuorum` |
| `non_promotable_voter_with_check_quorum` | `TestNonPromotableVoterWithCheckQuorum` |
| `disruptive_follower_prevote` | `TestDisruptiveFollowerPreVote` |
| `leader_start_replication` | `TestLeaderStartReplication` |
| `leader_commit_entry` | `TestLeaderCommitEntry` |
| `leader_acknowledge_commit` | `TestLeaderAcknowledgeCommit` |
| `leader_commit_preceding_entries` | `TestLeaderCommitPrecedingEntries` |
| `follower_commit_entry` | `TestFollowerCommitEntry` |
| `follower_check_msg_app` | `TestFollowerCheckMsgApp` |
| `follower_append_entries` | `TestFollowerAppendEntries` |
| `leader_sync_follower_log` | `TestLeaderSyncFollowerLog` |
| `leader_only_commits_log_from_current_term` | `TestLeaderOnlyCommitsLogFromCurrentTerm` |
| `handle_msg_app` | `TestHandleMsgApp` |
| `handle_heartbeat` | `TestHandleHeartbeat` |
| `handle_heartbeat_resp` | `TestHandleHeartbeatResp` |
| `msg_app_resp_wait_reset` | `TestMsgAppRespWaitReset` |
| `log_replication` | `TestLogReplication` |

Not ported: etcd tests that need snapshots, learners, configuration changes, or
leader transfer, and tests that set private state (`TestLeaderIncreaseNext`,
`TestSendAppendForProgressProbe`, `TestRecvMsgBeat`). The unit tests in `crates/raft`
cover the single-node cases (`TestCandidateConcede`, `TestStepIgnoreOldTermMsg`,
`TestAllServerStepdown`, `TestCampaignWhileLeader`, `TestPastElectionTimeout`).
