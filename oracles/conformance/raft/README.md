# Raft conformance

Election scenarios ported from the tests of etcd/raft
(<https://github.com/etcd-io/raft>, commit `1c0011d2c6b7`, Copyright 2015 The etcd
Authors, Apache License 2.0). `crates/raft` runs them as its `conformance` test. The
`[[test]]` entry in `crates/raft/Cargo.toml` is part of this oracle: to remove it is
to weaken the oracle.

```sh
cargo test -p raft --test conformance
```

## Port rules

- Each scenario keeps the name and the steps of its etcd source.
- Foundation's Raft always runs PreVote and CheckQuorum. etcd runs some scenarios with
  one of them off. Such a scenario is adapted, and its comment states the difference.
- This phase has no log replication. A new leader sends a heartbeat where etcd sends
  an append, and a scenario sets a node's last log position directly.
- A scenario reaches a state only through the public surface of `raft`.

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

Not ported: etcd tests that need log replication, learners, or leader transfer, and
tests that set private state. The unit tests in `crates/raft` cover the single-node
cases (`TestCandidateConcede`, `TestStepIgnoreOldTermMsg`, `TestAllServerStepdown`,
`TestCampaignWhileLeader`, `TestPastElectionTimeout`).
