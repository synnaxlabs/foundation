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
  `Change` (the leader, the position of a configuration entry, its voters, and the
  keys it binds; RAFT VOTERS). `raft` owns the rule that gives each signature its
  claim: a proof entry claims the proof's grant to its candidate in the term of the
  message or hard state, a granted reply claims its grant from the sender to the
  receiver in the message's term, and a configuration entry claims its change from the
  leader whose votes it holds. `Claim::signer` is the node whose signature a claim needs
  (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032591908).
  `raft::Change.public_keys` holds the public key of each voter that `Voters::added`
  gives, and none in a leave entry. `Raft::propose_voters` takes a map of each voter of
  the new set to its key, and the joint entry binds the keys of the voters that it
  adds. `raft` keeps the keys with the entry and sends them in each link, and does not
  read them in its walk. The leader's signature of the change covers them (architect,
  #1106, 2026-10-19T12:43:33Z:
  https://github.com/synnaxlabs/foundation/issues/1106#issuecomment-6267490155).
  `Voters::added` gives the voters that a change adds, the one rule for `raft` and for
  the byte form, and the field is `public_keys`, because a bound is a limit in this repo
  (architect, #3380, 2026-10-19T14:59:43Z:
  https://github.com/synnaxlabs/foundation/pull/3380#issuecomment-6270310245).
  `raft` gives this node's own entries, grants, and changes with no signature
  (`None`).
  `Ready::sign` gives each `None` the signature that the caller's closure makes for
  its claim, in the hard proof, in each message, and in each change this node wrote
  (in `entries`, in `committed`, and in each append), before the write and the
  sends. `raft` keeps each claim that it makes unsigned, and a claim that it reads at
  start keeps its signature. So `Ready::sign` signs each distinct unsigned claim of a
  `Ready` once, also when many messages carry it, and a resend in a later `Ready`
  again; Ed25519 gives each copy the same bytes (`laptop.architect`,
  2026-10-19T13:53:13Z,
  https://github.com/synnaxlabs/foundation/issues/3140#issuecomment-6268881124, and
  2026-10-19T14:40:23Z,
  https://github.com/synnaxlabs/foundation/issues/3375#issuecomment-6269894725).
  Supersedes the signature of each copy of
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6041049761. A data
  entry carries no claim, so the cost does not grow with the data rate. The most
  frequent case is a leader with a peer that does not answer: each heartbeat to it
  carries the votes and the chain, so the leader signs each unsigned claim of them once
  for each tick (100 ms), for all such peers together. A node that
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
  may send. `step` drops a reply with no check when its sender is neither in
  `voters()` nor a learner (`Raft::set_learners`), unless the configuration in force
  removed the sender or a later call to `set_learners` left it out, and the node has
  not released it yet: only `raft` knows whom it asked (#352). A learner's reply moves
  only its own progress. A node that may send can stop a group for good with one message
  in term `u64::MAX`: each node writes that term, and none can campaign. `raft` takes
  the term as it is. It trusts its voters: one that lies can already break safety,
  because a
  false `AppendReply` counts as held, so a bound on the term would guard nothing. No
  bound on a term jump spares an honest node that was down, either. Later: a sender
  proves a term jump by a signed term, which needs `mesh` (#750). The person decided on
  2026-10-05 ("(a) is fine", #352 item 2). When the term of the last entry is above
  `hard.term`, `Raft::new` starts at that term with no vote.
  The node sends nothing before its write, so no peer counted a vote or an answer that a
  lost `hard` held. The caller writes `hard` and `entries` in any order, with no atomic
  write. Lost: the `Ready` doc requires `hard` before `entries`, a patch that each
  caller must keep and that shows only at a restart. The person decided on 2026-10-05
  ("I approve long term fix on 522"), #522. `Raft::admission(&Message) -> Admission`
  says how `mesh` takes a message from another node: `Admitted` for a reply or a
  request of a voter of the newest configuration, `Removed` for a `PreVote` or a `Vote`
  whose last index is at or below the commit, or whose last term is below the term at
  the commit (`laptop.architect`, 2026-10-19T18:56:00Z:
  https://github.com/synnaxlabs/foundation/pull/3407#issuecomment-6274855859), from a
  node that a committed configuration removed (the configuration before the entries or a
  committed `Voters` entry held it, and the last committed configuration lacks it), and
  `NotVoter` for each other request. `mesh` keeps no copy of the configurations (#1105,
  #1762). It replaces `Raft::removed` (decided by `laptop.architect`,
  2026-10-19T12:57:24Z:
  https://github.com/synnaxlabs/foundation/issues/1107#issuecomment-6267749373).
  `Raft::removed_by(by) -> Option<BTreeSet<node::Key>>` takes a `removed` answer from
  voter `by`, and gives each voter whose answer counts once they are a majority of the
  other voters of the incoming half of the newest configuration in the log. An answer
  counts for `2 × election_ticks` ticks, and the answers clear when the configuration
  in force changes. An answer from any other node changes nothing (decided by
  `laptop.architect`, 2026-10-19T18:47:21Z:
  https://github.com/synnaxlabs/foundation/pull/3407#issuecomment-6274691760; the text,
  2026-10-19T21:47:53Z:
  https://github.com/synnaxlabs/foundation/pull/3407#issuecomment-6278129919).
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
  `Voters::quorum` and `NoMajority` are public: `mesh` counts the holders of a spec
  change as `raft` counts a commit (#1875). Decided by `laptop.architect`,
  2026-10-08T11:53:51Z, item 4:
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643.
  `Config::fits` says whether the caller can send a message to each peer. An `Append`
  with no proof drops its last entries until it fits, down to one, and the progress of
  the peer records the last entry that stays. A message with a proof drops its entries,
  then the oldest links of its chain, until it fits. `raft` panics when `fits` is false
  for an `Append` of one entry with no proof, or for a message with no entry and no
  link: that is a defect of the caller, which must bound each entry it proposes and each
  entry of an `Append` that it steps. A shorter chain is safe: the chain is input to a
  check that does not change, so a sender that sends less cannot make a receiver accept
  more. A receiver whose commit index is below the oldest link that fits never proves
  the term (#2471). `Config` is not `PartialEq`: a function pointer has no reliable
  equality. Lost: the caller sends only the entries that fit, which moves the peer's
  next index past entries that it lacks; a limit for each peer, which holds a value of
  the transport that changes with each session; a byte bound and a size function in
  `Config`, two fields where one predicate says the same. Decided by `laptop.architect`,
  2026-10-12T18:29:52Z
  (https://github.com/synnaxlabs/foundation/issues/1361#issuecomment-6130215772), with
  the deferral of #2471. A message with a proof takes the links from the newest back
  while they fit with no entry, and clones only those and the first one that does not
  fit. An `Append` keeps its entries only when the whole chain fits with no entry.
  This gives the message that a drop of the oldest link at a time gives, under the two
  conditions of `fits` in its doc. The `mesh` length is a sum, so it meets both. A
  leader with 127 silent learners after 300 voter changes uses 17.2 to 18.5 ms/s of
  idle CPU (box2, release, sim driver), in place of 134 to 136 ms/s with a clone of
  each link and a drop of the oldest one at a time: the cost of a fault, over the idle
  limit of 10 ms/s. Lost: a binary search on the count of links to drop, which clones
  each link of the log first; links that the queues share, the deferral "entries that
  the queues share, not copy" below. Decided by `laptop.architect`,
  2026-10-20T00:59:40Z
  (https://github.com/synnaxlabs/foundation/issues/3140#issuecomment-6280820710).
  `Raft::set_learners` sets the nodes that get the log and have no vote. `raft` keeps
  the set apart from the configuration: a key that the configuration in force holds as
  a voter is a voter. A leader sends each learner the log and the commit index, and
  never counts its answer toward a commit, an election, or the quorum check. A
  learner stays until a later call leaves it out: neither the quorum check nor a
  change of the configuration releases it. A node that a call leaves out and
  that no configuration in force holds is released as a removed node is. A node never
  sends a learner of its set a PreVote or a Vote. Each node keeps the set, so a new
  leader has it. `mesh` is to set it from the members of its applied state, PR 2 of
  #3140. Lost: learners in the configuration, which puts them in each signed change;
  `Config::learners` at `Raft::new` with no setter, because `mesh` learns of a member
  only at an apply. Decided by `laptop.architect`, the plan, 2026-10-19T13:53:13Z
  (https://github.com/synnaxlabs/foundation/issues/3140#issuecomment-6268881124), and
  the surface, 2026-10-19T14:40:23Z
  (https://github.com/synnaxlabs/foundation/issues/3375#issuecomment-6269894725).
  A leader keeps at most one append in flight to a learner (8 to a voter). A heartbeat
  reply frees no slot of a learner. When a learner answers this leader during a whole
  quorum check, but answers no append, the leader sends to it again from its accepted
  index, or sends its probe again, so a lost append goes again within two quorum checks,
  also when no new entry comes. A learner that answers nothing keeps its window and gets
  no resend. A learner whose append takes longer than a quorum check can get the same
  entries again about each second check. A leader sends each learner a message at least
  once each `election_ticks` of its own ticks: a heartbeat at each quorum check, not
  each `heartbeat_ticks`. A learner never votes or campaigns, so it needs no heartbeat
  at the rate of an election. A leader sends a learner the commit index at the next tick
  after the commit index that it can take moves: in its next append with entries, or
  else in a heartbeat. A leader sends a learner that a call adds an append at once,
  unless an append to it is in flight or the learner holds the whole log, so its
  catch-up does not wait for a quorum check. Decided by `laptop.architect`,
  2026-10-19T19:01:39Z
  (https://github.com/synnaxlabs/foundation/issues/3140#issuecomment-6274961164), for
  the cost of 127 learners; the resend and the append at once to a learner that a call
  adds, 2026-10-19T19:48:34Z
  (https://github.com/synnaxlabs/foundation/pull/3415#issuecomment-6275865221) and
  2026-10-19T19:51:09Z
  (https://github.com/synnaxlabs/foundation/pull/3415#issuecomment-6275912627); the
  exceptions of an append in flight and of a learner that holds the whole log,
  2026-10-19T21:41:27Z
  (https://github.com/synnaxlabs/foundation/pull/3415#issuecomment-6278021695); the
  resend of an expired probe at its `next`, 2026-10-19T20:07:40Z
  (https://github.com/synnaxlabs/foundation/pull/3415#issuecomment-6276219408); the
  message each `election_ticks` and the commit, 2026-10-19T19:46:05Z
  (https://github.com/synnaxlabs/foundation/pull/3415#issuecomment-6275819044). A bound
  that does not grow with the learners (entries that the queues share, not copy) is
  deferred. Trigger: before `MEMBERS_MAX` grows past 128. Supersedes the pace of a
  learner "as it does a voter" of the surface, 2026-10-19T14:40:23Z
  (https://github.com/synnaxlabs/foundation/issues/3375#issuecomment-6269894725).
  `Message::replaces(&self, older)` tells the caller that `older`, a message that has
  not gone yet, can drop when this one goes after it. Both have the same sender and the
  same receiver, and one of these holds: `older` has a lower term, and this one is not a
  `PreVote` or a `PreVoteReply`, whose term can differ from the sender's own; or both
  are a `Heartbeat`, both a `PreVote`, both a `Vote`, both a `HeartbeatReply`, or both a
  `PreVoteReply` with the same answer, and `older` has the same term or a lower one.
  `raft` owns the rule, and the caller's queue applies it, so the queue of a node that
  does not read holds one heartbeat, not one for each tick, and a message of a term that
  its sender left drops when the sender sends that node a message of a later term. Lost:
  a receiver still at the lower term can apply later, when the commit index that `older`
  named comes in an append of the new term. Decided by `laptop.architect`,
  2026-10-19T19:31:21Z
  (https://github.com/synnaxlabs/foundation/issues/3140#issuecomment-6275544916), for
  the lower term and the requests of a vote, 2026-10-19T19:53:12Z
  (https://github.com/synnaxlabs/foundation/pull/3417#issuecomment-6275950285), and for
  the replies to a heartbeat and a pre-vote, 2026-10-19T20:33:44Z
  (https://github.com/synnaxlabs/foundation/pull/3417#issuecomment-6276712034), and for
  its cost, 2026-10-19T19:56:42Z
  (https://github.com/synnaxlabs/foundation/pull/3417#issuecomment-6276015270, finding
  3).
