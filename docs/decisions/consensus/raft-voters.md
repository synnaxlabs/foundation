- **RAFT VOTERS (#193)** `Start.voters` is a `raft::Voters { incoming, outgoing }`,
  the etcd joint configuration: `incoming` is the voter set, and `outgoing` is the set a
  joint phase replaces, else empty. An election, a commit, and a leader's quorum check
  need a majority of each non-empty set. Each set is a `BTreeSet`, so a duplicate cannot
  exist and the order is fixed. An empty `incoming` with an `outgoing` is
  `Error::EmptyIncoming`; both empty is a node that only follows. etcd's quorum tables
  are the oracle for the quorum math (`oracles/conformance/raft/quorum/`). A node only
  in `outgoing` still campaigns, so a leader keeps its lead through its own removal. A
  configuration travels in the log: `Entry.data` is a `raft::Data`, one of `Empty` (a
  leader's first entry of its term), `Bytes` (a proposal), or `Voters(Change)`. A
  `Change` is the `voters`, the `votes` of the leader that wrote the entry (its election
  proof as it held it at the write: a vote that arrives later joins the leader's proof,
  not an entry it already wrote), and the leader's `signature` of the entry (`None`
  until `Ready::sign`). A node that missed the change checks the entry with them as a
  link of a chain before it counts a later proof against it, and refuses a link whose
  votes are not `Vote` (RAFT SURFACE; architect, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z). A node uses the latest `Voters` entry in its log from the time
  it writes it; `Start.voters` is the configuration before `Start.entries`. A node that
  joins starts with the founding voters from the answer to its join (decided by the
  architect, #242:
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6030855135). An empty
  `Start.voters` is a voter that an operator wiped. It takes any proof until it holds a
  `Voters` entry (#1004). Then its first `Voters` entry shows the configuration before
  the entries: a joint entry's outgoing set, or for a leave its own set (#928,
  coordinator, 2026-10-06). A log starts at index 1, so that entry is the joint entry of
  the group's first change, and the node checks proofs as a founder with the same log
  does, gaps included (#881, #1005). Lost: an empty committed set proves nothing (the
  node then refuses a leader that the outgoing set elects when the old leader fails
  before the joint entry commits); a joining node starts with the group's current
  configuration (the caller must know it, and it removes the operator's recovery of a
  wiped voter); the founding configuration as entry 1, as in etcd (a wider change that
  alone leaves the node open until it holds that entry). A `Voters` entry with an empty
  `incoming` set, in `Start.entries` or in an `Append`, is `Error::NoVoters`: a group
  with no voter can never commit or elect. A leader changes the voters with
  `Raft::propose_voters(set)`: it writes the joint configuration (`incoming` the new
  set, `outgoing` the current one) and, when that entry commits, the leave (`incoming`
  alone). One change at a time: while the last configuration entry is not committed, a
  proposal is `Error::ChangePending`. A node the change removed stays a peer of the
  leader, and gets appends up to the leave, or up to the leader's first entry when that
  is later, until it holds them and the leave is committed: then the leader sends it the
  commit in a heartbeat and releases it, so the node learns it is out and never
  campaigns. The leader's first entry replaces each entry that an older leader left past
  the leave on the node, such as a configuration that makes it a voter again. A removed
  node that answered nothing over a whole quorum check period is released at that check
  instead, and the next configuration releases any that is still a peer. A follower
  releases the removed nodes when the leave commits. A removed node that missed its
  release learns it from `mesh`, not `raft`: `mesh` admits a `raft` request only from a
  voter of the newest configuration that the node knows: the newest in its log, or a
  newer one that a link of the request's chain proves. A link proves a configuration
  when the votes it carries elected its leader under a configuration the node already
  knows, and the leader's signature holds (#881). A configuration entry binds the public
  key of each voter it adds, under the signature of the leader that writes it. A node
  answers `removed` only to a sender that a configuration in its own log held, when its
  committed configuration lacks the sender and the request proves no newer configuration
  that holds it. Each other sender that is not a voter gets `Error::NotVoter`, and does
  not stop. The person chose A, 2026-10-07T04:49:33Z
  (https://github.com/synnaxlabs/foundation/issues/1096#issuecomment-6031153921; the
  text, https://github.com/synnaxlabs/foundation/issues/1096#issuecomment-6031072285).
  Supersedes the first version, which the person approved on 2026-10-06
  (https://github.com/synnaxlabs/foundation/pull/647#issuecomment-6007546638). #1105
  builds the `removed` answer and the held rule, #1106 the key binding, and #1107 the
  configuration that a chain proves. Until #1107, a request proves no newer
  configuration. A node answers `removed` only to a sender that `Start.voters` or a
  committed `Voters` entry held, when the last committed configuration lacks it. A
  sender that only an uncommitted entry held gets `NotVoter`: the entry can still be
  truncated, and a node whose commit lags would stop a voter that no committed
  configuration removed, the failure of #1054 (decided by the architect,
  2026-10-08T02:21:15Z:
  https://github.com/synnaxlabs/foundation/issues/1105#issuecomment-6050855747). `mesh`
  asks `Raft::removed`, so the log that holds the configurations answers it (decided by
  `laptop.architect`, 2026-10-08T03:04:33Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051316777).
  `Raft::removed` counts a configuration entry only once the commit index covers it, so
  a node that opened again answers `NotVoter` until a leader gives it the commit index.
  A request is a PreVote, a Vote, a heartbeat, or an append. The rule covers requests
  only, and `raft` decides which replies count (RAFT SURFACE). The coordinator gives the
  person's words on the first version in its comment on #647, linked above. The removed
  node takes that answer only from a voter of its own region, and stops its `raft` group
  for that region. A voter of its region is a voter of the newest configuration in its
  log, and an answer from any other node drops the stream, as any refusal does (decided
  by `laptop.architect`, 2026-10-08T02:59:07Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051260164). `raft`
  sends such a node no entries, only answers. A voter with a lease drops its campaign or
  refuses it with a `PreVoteReply` of `Answer::Refused` at the voter's term. In `raft`
  alone, the node campaigns. While a voter has a lease, this has no effect. Once no
  voter has a lease, as after the leader fails, the voters can elect the node: it
  commits an entry of its term, which commits the leave, and steps down, and the voters
  follow it until their election timeout. That gap stays in `raft`, pinned by
  `it::change::the_voters_elect_a_removed_node_once_the_leader_fails`. In `mesh`, the
  admission check and the `removed` answer close it for each voter whose log holds the
  leave; a voter whose log lacks the leave entry admits the request until #1107. #483
  keeps the stop on applying a committed configuration without itself. Keeping readmit
  until then lost. The person decided on 2026-10-05 ("(a)"), #482. Readmit in `raft`
  (#414) lost: it sent the log to a sender that `raft` cannot check. The person decided
  on 2026-10-05 ("Ok B is fine", #193). A leader outside the committed final set sends
  the commit and steps down. A node may campaign when it is a voter, incoming or
  outgoing, of the configuration in force, or, while that configuration is not
  committed, of the configuration before it. No other node campaigns. The rule is exact:
  a leader appends a configuration entry only after the last one commits, so by Log
  Matching only the last configuration entry in a log can be truncated, and the one
  before it is committed. The fallback keeps two cases: a truncation gives the
  configuration before back, and a removed leader that lost its lead before the leave
  reached a peer is the only node that can win the election that commits it. A follower
  whose commit index lags lets the configuration before campaign for longer, which costs
  liveness, never safety. The `mesh` admission check stays beside this rule:
  `promotable` decides whether an honest node campaigns, and `mesh` checks a sender that
  may lie, because `raft` never checks senders (RAFT SURFACE). Neither is a second guard
  for the other. Decided by the coordinator and the advisor on 2026-10-06 (#659). After
  compaction a snapshot carries the configuration in force at its index, so the
  configuration before the last entry stays known (#253).
  `mesh` keeps the founding voters with the rest of `Config::founding` at the first
  open of its directory, and refuses another set at a later open (MESH DRIVER, #1209).
