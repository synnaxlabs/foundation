- **RAFT SURFACE (#5, #91)** `raft::Raft::new(Config, Start)` builds a follower.
  `Config` holds the fixed inputs (key, tick counts). `Start` holds what the node had
  on disk: `hard`, `voters`, `entries` (the log from index 1), and `applied` (the
  last index the caller applied). `Hard` holds the term, the vote, the leader of the
  term (this node when it led), and the proof that moved the node to the term: its
  own pre-votes when it campaigned, else the proof of the message that moved it. A
  `Proof` is a `Grant` (pre-vote or vote), the candidate, and each voter's key with its
  `Signature`, the candidate included. It proves the term of the message or hard state
  that holds it. A granted `PreVoteReply` or `VoteReply` carries the voter's signature
  in its `Answer`, and the candidate copies it into its proof. `raft` counts the keys
  and carries the signatures as opaque bytes: it does no crypto. A signature attests
  a `Claim`: a `Grant` (the voter, the grant, the term, and the candidate) or a
  `Change` (the leader, the position of a configuration entry, and its voters; RAFT
  VOTERS). `raft` owns the rule that gives each signature its claim: a proof entry
  claims the proof's grant to its candidate in the term of the message or hard
  state, a granted reply claims its grant from the sender to the receiver in the
  message's term, and a configuration entry claims its change from the leader whose
  votes it holds. `Claim::signer` is the node whose signature a claim needs
  (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032591908).
  `raft` gives this node's own entries, grants, and changes with no signature
  (`None`).
  `Ready::sign` gives each `None` the signature that the caller's closure makes for
  its claim, in the hard proof, in each message, and in each change this node wrote
  (in `entries`, in `committed`, and in each append), before the write and the
  sends. `raft` keeps each claim that it makes unsigned, and a claim that it reads at
  start keeps its signature. So `Ready::sign` signs each copy of an unsigned claim that
  a `Ready` holds, and a resend again. Ed25519 gives each copy the same bytes. A data
  entry carries no claim, so the cost does not grow with the data rate. The most
  frequent case is a leader with a voter that does not answer: each heartbeat to it
  carries the votes, so the leader signs once for each tick (100 ms). A node that
  campaigned for its term signs each refusal of a lower term in the same way, because
  the refusal carries its own pre-votes. Lost: `raft` keeps the signed copy, which puts
  the signer in `raft` and changes the conformance oracle (architect, #1187,
  2026-10-07T15:25:42Z,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6041049761).
  The caller checks each pair that `Raft::claims` gives before `step` and
  refuses a `None`: `step` keeps each signature as it came, so an unchecked `None`
  of another voter reaches `Ready::sign`. `Raft::claims` gives the claims `step`
  reads, in its order: the proof's grants, each link of the chain that `step` reads
  (its votes in the link's term, then the change), each change an append carries
  (its votes in the entry's term, then the change), then the sender's grant. It
  gives no claim of a message for a lower term or of a reply from a node that is not
  a peer, which `step` does not check. The list is the one `step` reads only when
  `step` gets the same message, with no call to the node between the two
  (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032381078,
  2026-10-07T06:32:04Z,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z, and
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6042831364,
  2026-10-07T17:08:02Z). Amended (approved by `laptop.architect`,
  2026-10-07T20:35:59Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046363822,
  2026-10-07T20:47:58Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046560645, and
  2026-10-07T20:51:39Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046617974,
  #1589): the rule is that a message `step` refuses or drops by its header gives no
  claim. So it also gives no claim of a message for another node, from this node, or
  from a second leader of this term (`Misrouted`, `Loopback`, `SecondLeader`). One
  predicate, `Raft::reads`, holds each refusal and drop by the header, and decides
  both. A grant or a proof that `step` reads past the header and then ignores is
  still a claim (#1613 holds the design that removes the class).
  `Entry::claims`, `Proof::claims(term)` and `Link::claims` give the claims of one
  entry, proof, or link in the same order, so `mesh` edits a message before `step`
  reads it (decided by `laptop.architect`, 2026-10-07T18:57:55Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6044730486, and
  2026-10-07T20:05:30Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6045861142).
  `Message.proof` carries one: a `Vote` carries the candidate's pre-votes; a leader's
  `Heartbeat` or `Append` carries its votes until the receiver answers an append, and
  again after the receiver is silent through a quorum check;
  an answer to a message of a lower term carries the sender's hard proof, and a node
  with no proof of its term sends no refusal. `step` checks a proof before anything
  changes. A `PreVote`, or a granted `PreVoteReply`, of a higher term needs none.
  Every other message of a higher term needs a proof that fits its body (a `Vote`
  the sender's pre-votes, a `Heartbeat` or `Append` the sender's votes, a reply any
  proof of the term) whose voters are a quorum of this node's configuration in
  force or last committed, or of a configuration that the message's chain proves;
  else `Error::Unproven`, and nothing changes or is sent. A leader claim in this
  node's own term follows RAFT LOG. A late pre-vote or vote of the term joins the
  proof its candidate carries. The chain: a message with a proof carries
  `Message.chain`, the configuration entries of the sender's log below the message's
  term, oldest first, each a `Link` (its position and its `Change`). A node whose
  configuration the proof is no quorum of reads the chain from its first link above
  its commit index, and stops at the first link whose configuration the proof is a
  quorum of. Each link it reads must have a term below the message's, rise from the
  position at the commit index or the last link read (the index rises, the term does
  not fall), hold `Vote` votes, hold a configuration with at least one incoming
  voter, and hold votes of a quorum of the configuration it trusts: the last link
  read of a lower term, else the node's last committed configuration entry of a
  lower term, else the configuration before its entries.
  The node keeps nothing from the chain: the leader's appends bring the entries. A
  voter that was down through a change so follows the leader that the change elected,
  and helps elect the next one (`raft/tests/it/behind.rs`). A node that took its term
  through another node's chain answers a stale message with its hard proof and its own
  chain, which can fall short of the sender's configuration: the sender then stays in
  its term until the leader's chain moves it, and the random runs check that a
  leader's heartbeat or append is never unproven (builder, #881, approved by the
  architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6043521735,
  2026-10-07T17:45:39Z). Known gap: a node that a leave removed can reach, through
  pre-votes of the old configuration, a term that no configuration entry stands
  behind; a change that adds it back then needs its ack, and no node passes its term.
  The random runs reject such a run; the fix is #1485 (architect,
  https://github.com/synnaxlabs/foundation/issues/1485#issuecomment-6042768573,
  2026-10-07T17:05:00Z). A link is attested by its leader's signature and the votes of
  its term alone, so a voter that led a term can sign a configuration entry it never
  wrote, and prove any term with it: `raft` trusts its voters until #882, which gives
  a link the signed acks of a quorum, and `prove` counts them. The test
  `a_voter_that_led_a_term_can_forge_a_link_to_itself_and_prove_any_term` pins the gap
  (architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6043096423,
  2026-10-07T17:22:17Z). The chain excludes a leader that a change the node missed
  made a voter (#1096). The log keeps the indexes of its configuration entries, so a
  chain costs their number, not the log's: the plan deferred the index until a
  measured scan on a large log (builder, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6040930256,
  2026-10-07T15:19:54Z), and the review of PR 2 measured a leader of 7 voters with
  1,000,000 entries and 6 silent peers at 41 to 44 ms per tick with the scan
  (reviewer,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6042889435,
  2026-10-07T17:11:00Z). With the index, the same tick, its `ready` included, takes
  0.33 µs (box1, Intel Xeon Platinum 8488C; `crates/raft/benches/chain.rs`). No
  test fails when `links` goes back to a scan: a scan allocates no more than the
  index, so no count sees it. The bench is the check until #715 gates it, and the
  gate's CI job must run it (builder, #881,
  https://github.com/synnaxlabs/foundation/issues/715#issuecomment-6044217325,
  2026-10-07T18:27:12Z; accepted by the architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6044211313,
  2026-10-07T18:26:50Z).
  The advisor required a proof on every message and on each refusal, signatures
  only, and the proof in the hard state (#750, 2026-10-05). `mesh` signs and checks
  the signatures (MESH LOG).
  `Raft` takes `tick(random)`,
  `step(message)`, and `campaign()`, and gives `ready()`: a `Ready` with `hard` (only
  when it changed), `entries` to write, `committed` entries to apply, and `messages`
  to send. The caller writes, then sends, then applies, as etcd does: to apply first
  only delays the next round trip. A candidate counts its own vote at once because
  the write comes before the send. `hard()` stays a getter like `term()`. Randomness
  enters only through `tick`: a node draws its election timeout on the first tick
  after a reset. PreVote and CheckQuorum have no off switch. A PreVote answer, grant or
  refusal, shows the voter's state when it sent the answer. A grant that arrives after
  its voter got a lease back still counts, and costs one needless election; safety
  holds. etcd/raft counts such a grant too. Lost: a round number in `PreVote`, which
  changes the message format and closes only the case of two pre-campaigns. Decided by
  the advisor under the failover delegation on 2026-10-05 (#719). A node that may not
  campaign (RAFT VOTERS) still votes and follows.
  `step` does not check that a sender is a voter (a voter can learn late
  that a peer joined), so the caller authenticates the sender and decides which nodes
  may send. `step` drops a reply with no check when its sender is not in `voters()`,
  unless the configuration in force removed the sender and the node still sends to it:
  only `raft` knows whom it asked (#352). A node that may send can stop a group for good
  with one message in term `u64::MAX`: each node writes that term, and none can
  campaign. `raft` takes the term as it is. It trusts its voters: one that lies can
  already break safety, because a false `AppendReply` counts as held, so a bound on the
  term would guard nothing. No bound on a term jump spares an honest node that was down,
  either. Later: a sender proves a term jump by a signed term, which needs `mesh`
  (#750). The person decided on 2026-10-05 ("(a) is fine", #352 item 2). When the term
  of the last entry is above `hard.term`, `Raft::new` starts at that term with no vote.
  The node sends nothing before its write, so no peer counted a vote or an answer that a
  lost `hard` held. The caller writes `hard` and `entries` in any order, with no atomic
  write. Lost: the `Ready` doc requires `hard` before `entries`, a patch that each
  caller must keep and that shows only at a restart. The person decided on 2026-10-05
  ("I approve long term fix on 522"), #522. `Raft::removed` says whether a committed
  configuration removed a node: the configuration before the entries or a committed
  `Voters` entry held it, and the last committed configuration lacks it. `mesh` is to
  ask it at a refusal and keeps no copy of the configurations (#1105, #1762).
  `Voters::contains` and `Voters::nodes` are public. Decided by `laptop.architect`,
  2026-10-08T03:04:33Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051316777. After
  compaction, a snapshot also carries the nodes that the configurations it replaces
  held or removed, so the answer survives a trim (#253; `laptop.architect`,
  2026-10-08T03:41:50Z:
  https://github.com/synnaxlabs/foundation/pull/1775#issuecomment-6051704590).
  Supersedes the place of `held` in `mesh` in part 2 of
  https://github.com/synnaxlabs/foundation/issues/1105#issuecomment-6050855747 and
  finding 2 of
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051260164.
