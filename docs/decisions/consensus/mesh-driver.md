- **MESH DRIVER (#471)** `mesh` runs the `raft` group of one region as one task, on the
  shard that opened it. The task waits for a tick or a `Ready`, and does each `Ready` in
  the order of RAFT SURFACE: sign, write and sync, queue the messages, apply. A ticker
  task and a writer task lost: they need a second waker. A tick is 100 ms, a heartbeat
  is 1 tick, and an election timeout is 10 ticks. A tick that comes due in a write is
  lost, so the group's time only slows. Before each `step`, `mesh` checks a message in
  this order: the peer holds the key of the member that the message names
  (`Error::Spoofed`), a request comes from a voter of this node's configuration
  (`Error::Removed` for a sender that a committed configuration removed, else
  `Error::NotVoter`), and each claim holds (`Error::Claim`), the claims being what
  `Raft::claims` gives, so a link that the node does not read is not checked. So a
  node with a configuration refuses a leader that is not a voter of that
  configuration, when a change that the node does not hold made that leader a voter.
  The node does not get the log from that leader (a known defect, #1096, that #1107
  fixes). A leader that stays a voter through the change passes the check, and its
  chain proves the change (RAFT SURFACE). A node with no
  configuration takes no request. Only a voter that an operator wiped is such a node
  (#881), because a node that joins opens with the founding voters from its join answer
  (decided by the architect, #242, 2026-10-07T04:20:40Z:
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6030855135). A claim
  in the proof whose signer has no key at the node, or whose signature does not hold
  under a key from a join that is not applied, is removed before `step`. An append is
  cut before the first entry with such a claim, and the entries after the cut are not
  checked or stepped. A claim of an applied member with a bad signature refuses the
  whole message (`Error::Claim` with `claim::Error::Forged`) (decided by
  `laptop.director`, 2026-10-07T20:02:05Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6045806233. This
  supersedes rule 3 of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6042828979, which
  superseded rule 3 of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423). In
  the chain, such a vote of a link is removed, and the chain is cut before the first
  link whose change is such a claim. `mesh` makes each change before `Raft::claims`,
  and steps the message that it checked (decided by `laptop.director`,
  2026-10-07T17:18:49Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6043037608). A
  grant of a reply that does not hold under the key of its sender refuses the reply
  (`Error::Claim` with `claim::Error::Forged`), also when the key comes from a join
  that is not applied: the sender check proved that the peer holds that key
  (decided by `laptop.architect`, 2026-10-07T20:37:34Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046390090).
  When the unapplied joins of a signer name two keys, its key is the key of the
  joins below the first configuration entry, in the log as `raft` holds it, whose
  incoming half names the signer, when those joins name one key, else none: the
  leader applied the real join before it wrote that entry, so Log Matching puts the
  real join below it in each log, and a join above it can be a forgery. The sender
  check and the claim check both use this lookup (decided by `laptop.director`,
  2026-10-07T20:44:24Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046503082.
  Supersedes the two-keys sentence of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038611630).
  The incoming half is enough: `raft` makes the outgoing half of an entry from the
  incoming half of the configuration in force, so a signer that only an outgoing
  half names is in the incoming half of an earlier entry, or of the applied
  configuration, and then its join is applied (decided by `laptop.architect`,
  2026-10-07T20:49:33Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046585323).
  Triggers: a change kind that removes a member, or a change to how `raft` makes
  the outgoing half, states this rule again. A link of the chain proves the entry
  in the log of its sender, not the entries below its position in the log of the
  receiver, so the lookup never reads a link as a configuration entry: when the
  two joins are in the log and the entry that names the signer is in the chain
  only, the vote of that signer is removed. With voters that do not lie, the limit
  is in liveness only, and #1623 is the sound fix, designed with #336 (decided by
  `laptop.director`, 2026-10-07T21:03:30Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046807570). A
  voter that lies can write a join with a key that it holds, and when that join is
  the only unapplied join of its node, the node takes that key, until #882 (decided
  by `laptop.architect`, 2026-10-08T00:52:26Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6049888903.
  Supersedes the sentence "the node never counts a wrong key" of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046807570).
  Two joins below that entry still strand a follower under a leader that the real
  node elected, until #336 builds the voter that checks a join before it stamps it.
  A hard proof that lost such a claim can be no quorum at a node with a newer
  configuration, which then learns the term from the leader. A follower answers a
  cut run with the last entry it kept, and the leader sends the rest from there.
  `propose_voters` refuses a set with a node that is not a member in the applied
  state of this node (`Error::NotMember`, the first such key), so each log
  that holds the `Voters` entry holds the join of each of its voters before it, and the
  join applies the same on each node (decided by `laptop.director`,
  2026-10-07T12:48:00Z and 2026-10-07T13:10:50Z, with the error of `laptop.architect`,
  2026-10-07T12:48:43Z and 2026-10-07T13:08:48Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423,
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038649429,
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038247134, and
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038611630).
  `propose` returns the position of its entry only after the write that holds the entry
  ends: a lone voter leads before its term is on disk, and after a power cut the same
  position can hold another change. A second call that waits for the write lost: no
  caller needs a position that is not on disk, and a caller that skips the wait gets
  that defect again (decided by the architect, 2026-10-07T08:18:51Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033920665). When the
  append of a new leader replaces the entry before a write holds it, `propose` gives
  "not the leader": the task tells each proposal whether the `Ready` that it wrote held
  the entry, by index and term, and the first `Ready` after the call decides (approved
  by the architect, 2026-10-07T10:17:54Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6035860357). A node
  that gets a forwarded proposal (MESH WIRE) proposes the change, and its answer is the
  position, or "not the leader" with the leader that it knows. A proposal from a peer
  whose key no voter of this node's configuration holds is refused
  (`Error::PeerNotVoter`, with the key of the peer, because a forwarded change names no
  sender; `Error::NotVoter` names the sender of a message; decided by the architect,
  2026-10-07T10:38:49Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6036181809); a
  member that is not a voter proposes with join (#336). The leader does not check the
  home of a forwarded change: `Error::NotMember` checks only the argument of a local
  caller, and the check of a home at apply on each node is #1273. A forwarded change
  applies at least one time: a member that got no answer forwards it again, and the
  leader then appends a second entry. A try of `set_home` that gives up resets its
  stream. A proposal that the network delivers late, before the reset, can still apply
  after a later call returned and set the older home, until #1273 refuses it (ruled by
  the architect, 2026-10-07T20:58:20Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046723849).
  Supersedes the sentence that a repeat of `Change::Home` gives the state of a call
  that took effect last:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025.
  `Change::Join` is safe to repeat while no
  change removes a member: a repeat finds its node a member and is refused
  (`Unfit::Duplicate`) before the ticket counts a use. The change that removes a member
  must keep a repeat of an older `Join` from admitting the node again, and needs a
  ruling before it lands (decided by `laptop.architect`, 2026-10-07T12:55:06Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6038355946). A later
  `Change` kind that is not safe to repeat needs a ruling before a member forwards it
  (decided by the architect, 2026-10-07T08:15:18Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025). The
  messages for one member wait in a queue of 64 that drops its oldest, because `raft`
  sends again. A write that finds the pool full (`block::Error::Exhausted`), or that the
  system refuses memory for (`Refused`), does not stop the group, because each may
  succeed later (MEMORY BOUNDS): the task writes the same `Ready` again at each tick,
  and until then no message leaves, nothing applies, and the group gets no tick. From
  the write that finds no block until the write ends, `propose` and `receive` give
  `Error::Pool` with the cause of the wait, so the group takes no proposal and no
  message, and what `raft` holds does not grow. A forwarded proposal that gets it did
  not reach the group. A leader that waits sends no heartbeat, so the other voters elect
  a new leader. A follower that waits answers no message and falls behind until its
  write ends (decided by the architect, #1091, 2026-10-07T05:29:41Z:
  https://github.com/synnaxlabs/foundation/issues/1091#issuecomment-6031627973; the doc
  text of the variant decided by the architect, 2026-10-07T14:17:49Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6039908458, which
  supersedes the doc texts of
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501 and
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823, and the
  Display text of the first (2026-10-07T11:52:00Z) stands; the text of the two cases by
  the architect, 2026-10-07T12:08:39Z:
  https://github.com/synnaxlabs/foundation/pull/1366#issuecomment-6037581525). The group
  checks the wait before each other check of a message or of a forwarded proposal, so
  each gets `Error::Pool` in a wait, also one that a check refuses with no wait. The
  other order lost: it gives the exact refusal, but the group drops each of them in a
  wait in both orders, and a node that is short of memory then also pays for the
  signature checks (decided by the architect, 2026-10-07T12:31:59Z:
  https://github.com/synnaxlabs/foundation/pull/1366#issuecomment-6037964937). A write
  holds one block of the pool at a time (MESH LOG), so no record is too large for a
  pool that opens, and a write does not wait for a block of its own (decided by the
  architect, 2026-10-07T08:42:04Z:
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034282653). A free
  block of a size with a block in use keeps its budget (#291), so a write can wait while
  the budget has room for its block, until the other user of the pool drops its block
  (#1134). A pool whose largest block is less than one sector does not open (MESH LOG),
  so no write gives `TooLarge` and the group does not stop for it (decided by the
  architect, 2026-10-07T06:32:47Z:
  https://github.com/synnaxlabs/foundation/pull/1123#issuecomment-6032389760; the
  `Refused` wait decided by the architect, 2026-10-07T04:39:07Z:
  https://github.com/synnaxlabs/foundation/pull/1057#issuecomment-6031046531). A group
  stops when a write of the log fails, when a committed change has 0 bytes or a kind
  that this build does not know, or when each `Mesh` drops: this build cannot judge such
  an entry, and a newer build can. An entry with no change (the first entry of a leader)
  is not a change of 0 bytes. A committed entry of a known kind whose body does not
  decode is `Refused::Body` on every node, and the group goes on, so one voter that
  proposes bad bytes cannot halt the region. From the first stable release (C9d), a
  change to the body or to a cap of a known kind (the 64 status entries of a `Join`)
  takes a new kind, which writers use only after the format flag (C9d) allows it; a
  node of an older build stops at it and never applies it differently. Decided by
  `laptop.architect` (2026-10-07T10:55:00Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036422521. The
  start of the rule was changed by `laptop.architect` at 2026-10-08T17:20:54Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065295958): before
  it, a format keeps version 1, so the homes of a spec change go in kind 4. It
  supersedes the start of the rule of
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036422521. Each later
  call gives `Error::Stopped` with the first cause. A watch gives the `Stopped` itself,
  also after each `Mesh` drops (MESH SURFACE). `member` has no error (#562): it gives
  the record that the node holds, also after a stop (approved by the architect,
  2026-10-07T08:07:48Z:
  https://github.com/synnaxlabs/foundation/pull/1241#issuecomment-6033747689). A stopped
  group does not start again: the node opens the mesh again, and the open makes durable
  what it gives (MESH LOG). The task ends soon after the last `Mesh` drops, a write in
  progress ends first, and a write that waits for a block ends at the next tick; until
  then a new open gives `Error::Log`. Each open applies the log from index 1, until
  snapshots (#253). A watch does not keep the group running, and a dropped watch leaves
  no waker. `open` refuses a node or a voter that is not a member (`Error::NotMember`),
  and a private key that is not the key of this node's member (`Error::WrongKey`).
  `Config.founding.members` is a list, and the region state holds each record under the
  key of its card, so the key of a member has one copy. `open` is the one check of a
  list for two records of one node: a decoder of a join answer passes its records on and
  does not check them again (decided by `laptop.architect`, 2026-10-07T08:07:47Z:
  https://github.com/synnaxlabs/foundation/issues/1259#issuecomment-6033747312, which
  reverses the map of the ruling below). The key of a member is the key that its card's
  signature covers, and `open` refuses a member that the region cannot hold, or two
  members with one key (`Error::Member`, with the `region::Unfit`). The signature does
  not show that the node owns its public key. The admission does, and `Join` (#336)
  refuses the `node::Key` of a member (decided by `laptop.architect`,
  2026-10-07T08:33:14Z:
  https://github.com/synnaxlabs/foundation/pull/1277#issuecomment-6034146773). `open`
  starts the tasks that send: one for each member, from the first message for it
  (approved by the architect, 2026-10-07T13:40:50Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6039206881, which
  supersedes the start at open in the plan that
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501 approved),
  so a member that is slow holds only its own messages. `mesh` dials and `node` accepts:
  `node` gives each stream of `wire::Protocol::Mesh` to `serve`. A task dials a session
  to each member at the addresses of the member's card, as the group holds the card
  then, and sends each `raft` message as one message of one one-way stream of
  `Class::Command`, after the stream header (MESH WIRE). `mesh` never closes a session.
  A message that fails drops, with only the part that failed: the message when the pool
  has no block for it or when it is too large for the peer (#1361), the stream when the
  peer stopped it, and the handle of the session on each other error, also when no dial
  gives a session. The next message then opens a stream, or dials, again. Nothing sends
  the dropped message again, because `raft` does. A local pool error must not drop a
  session that other protocols use, and the one session for each pair of nodes is the
  job of `transport` (#1363) (approved by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A
  message for a node of which the group has no record drops in the same way (approved by
  the architect, 2026-10-07T17:29:35Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6043221155). The tasks
  end when the group stops or when each `Mesh` drops, also a task that waits in a dial
  or in a send. The task of a voter that a change removed, to which `raft` sends no more
  messages, ends only then (#1401) (approved by the architect, 2026-10-07T13:40:50Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6039206881). The group
  holds the handle of the session to each member, and not the task, so that `set_home`
  (PR 4c-2 of #471) can open its stream on it: the task is the only one that dials (the
  plan of PR 4d,
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037266854, approved
  by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A stop
  of the group drops each handle. When `Transport::dial` gives the one open session to a
  peer (#1363), a call can take its session from `dial`, and #1598 decides whether the
  handle goes back to the task. `set_home` makes a member the home of an index, and
  returns when this node applied an entry that sets it. A try starts with two checks on
  this node: the node is a voter (`Error::NoVote`), and the home is a member
  (`Error::NotMember`). Only the two and a stop of the group end the call with an error.
  A voter of one half of a joint configuration is a voter here, as in the leader's check
  of a peer (`Error::PeerNotVoter`). The try proposes on this node. When another node
  leads, the try sends the proposal on a new two-way stream of the session to the leader
  that the group holds, and reads one answer. The call does not dial: two dialers for
  one peer need a rule for which session stays, and a follower sends to its leader in
  each tick, so with no session the leader cannot be reached. A try gets no position
  when no leader is known, when the group has no session to the leader, when the leader
  refuses the proposal, when the stream fails, when the pool has no block for the
  proposal or this node's group gives `Error::Pool`, and when this node's `raft` names
  another leader or term before the answer. The call then waits one tick and starts the
  next try. A try that waits for the answer has no time limit: the leader answers each
  forwarded proposal or ends its stream, and a session that fails one way ends at the
  timeout of `transport`. `Group` wakes each call at each change of the leader or the
  term, and at a stop, so a stop ends the wait at once. A limit of one election timeout
  on the answer lost: on a link with a round trip above it, `raft` keeps its leader,
  and each try gave up after the leader took its proposal, so the call appended one
  entry for each try and never returned (decided by `laptop.architect`,
  2026-10-07T22:40:51Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6048332653).
  Supersedes the limit of one election timeout in the plan that this approval took:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407. It also
  supersedes point 3 of
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552, a stop
  that ends a forward at most 11 ticks late. With a position,
  the call waits with no time limit until this node applied the entry of that term at
  that index, or until the log has a different entry there, and then it proposes again:
  a new leader commits an entry of its term, which decides each older position. A time
  limit lost: a slow group gets the change again at each timeout. A `request` number
  with a map of open requests lost: the stream is the request (MESH WIRE). `Applied`
  keeps the term of each applied entry only above the lowest floor of an open try, and
  the term of the last entry, so it holds one pair while no call waits (MEMORY BOUNDS).
  A dropped call leaves no waker and no floor, and its stream stops. A node that is not
  a voter gets `NoVote` and does not wait, because the leader gives it only code 16,
  which a full pool also gives (approved by the architect with two changes, `NoVote` and
  the bound of `Applied`, 2026-10-07T11:54:56Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407). The
  `NoVote` check reads the configuration of this node's log, which changes when the node
  appends a change of voters, before the commit. A promoted node gets `NoVote` until it
  appends that change, and a new leader that replaces the entry changes the result back.
  `NotMember` reads what this node applied (ruled by the architect,
  2026-10-07T20:58:20Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046723849).
  Supersedes the sentence that a promoted node gets `NoVote` until it applies that
  change: https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407.
  `Ok`
  means that the entry at the position of the try applied. That is an entry that sets
  the home only while `State::apply` never refuses a home change. A change kind that
  lets `apply` refuse a home change (such as a removal of a member) must also make
  `set_home` tell a refused entry from one that set the home. The surface as built, the
  doc of `set_home`, and three points that the plan did not state (a joint
  configuration, `Error::Pool`, and `NoVote` before `NotMember`) are approved by the
  architect, 2026-10-07T20:29:07Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552. The
  ruling of 2026-10-07T20:58:20Z above adds the sentence on a late proposal to that
  doc. Its sentences on what `NoVote` and `NotMember` read supersede the last sentence
  of the doc text in that comment ("The first two read what this node applied").
  Proposed by box1.builder-3, decided by the architect (#471),
  2026-10-07T04:11:26Z:
  https://github.com/synnaxlabs/foundation/pull/1057#issuecomment-6030753391.
  Amended (2026-10-08, the `mesh` PR before PR 3b of #585): each task that sends ends
  at once after the group stops or the last `Mesh` drops. A dial or a send in
  progress stops, so it never holds `Mesh::ended` for a dial timeout. Decided by
  `laptop.architect`, 2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643.
  Amended (2026-10-08, PR 3b of #585): the mesh's directory is `mesh` in the data
  directory. Decided by `laptop.architect`, 2026-10-08T03:37:20Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475. `node`
  gives it as `mesh::Config::dir`. Decided by `laptop.architect-2`,
  2026-10-08T03:54:37Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866, and
  approved by `laptop.architect`, 2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643.
  Amended (2026-10-08, PR 1 of #1741): with a region, `node` opens the chunk store in
  `blob` in the data directory before the mesh, and gives it as
  `mesh::Config::store`. Approved by `laptop.architect-2`, 2026-10-08T11:50:17Z:
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059221889. The name
  `store`: `laptop.architect`, 2026-10-08T11:53:51Z, item 5 of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643. A store
  that does not open stops the node with `Error::Blob`. Decided by
  `laptop.architect-2`, 2026-10-08T11:50:51Z:
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059230722.
  Amended (2026-10-08, PR 2 of #1209): the first open of the mesh directory keeps
  `Config::founding` in the file `founding` in `Config::dir`, beside `log`, so the
  `Stray` rule of MESH LOG holds. The file is an 8-byte check of the rest (the first
  bytes of `types::digest::Digest::of`), the format version (1), and
  `region::Founding::encode`: the prefix, a count and each member in key order, the
  voters, a count and each definition in name order, and a count and each home in
  channel key order. An open whose log holds no record is a first open: it takes the
  lock of the log, removes `founding`, writes `founding.new`, renames it to `founding`,
  and syncs the directory, before `raft` writes a record. So a crash before the first
  record leaves a first open, also with another founding (`laptop.architect`,
  2026-10-09:
  https://github.com/synnaxlabs/foundation/pull/2200#issuecomment-6091088392). Each
  later open compares `founding` with `Config::founding`, its
  members in key order, before `raft` starts. Another value gives
  `Error::Founding { stored, given }`, each a `Box<region::Founding>`. Its text names
  the first field that differs: the prefix and the voters in the form "the mesh was
  founded with voters {stored}, not {given}", and for the members, the definitions,
  and the homes the first key in key order whose value differs or is in only one of
  the two. A text of the homes names the index by the tree key of its
  `Definition::Channel` in `stored`, or by its key when no definition has it: "the
  mesh was founded with another home of index {name}", "... with a home of index
  {name}, which the config lacks", and "... with no home of index {name}"
  (`laptop.architect`, 2026-10-09:
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6090902577). A
  log with a record and no `founding`, or a `founding` that fails its check, its
  version, or its decode, gives `Error::Unfounded { path }`. A failed file
  call on `founding` gives `Error::Files`, and a pool with no block for it gives
  `Error::Pool`, not `Error::Log`, which names a part that did not fail
  (`laptop.architect`, 2026-10-08T16:10:01Z:
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6064069802). This
  supersedes `Error::Voters` of the rules of #1209 and the digest form
  `Error::Founding { stored: Digest, given: Digest }` of
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6056362876, because
  the whole value is the unit of the check (`laptop.architect`, 2026-10-08T10:34:50Z:
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6057978189).
  Proposed by `box1.builder-4`
  (https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6063177321), and
  decided by `laptop.architect`, 2026-10-08T15:26:10Z, with the text for the members
  and the definitions:
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6063246600. The
  lock of the log comes before the write of `founding`, not after it as in the plan, so
  two opens of one directory never write it at once. Lost: a digest of the whole value
  in the file, because the operator cannot see which field differs and the region does
  not read back; one variant per field, four variants for one contract; the root of the
  definitions alone, because the file is then not the whole value.
