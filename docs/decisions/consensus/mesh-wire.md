- **MESH WIRE (#471)** `mesh` encodes what two nodes of a region say on a stream of
  `wire::Protocol::Mesh`, behind the `wire` stream header: a `raft::Message`, a proposal
  that a follower forwards to the leader, and its two answers (the position of the
  entry, or "not the leader" with the leader the receiver knows), and a join request
  and its answers (`Unjoined`, or `Joined` and the `Founding` parts, MEMBER RECORD).
  `wire` does not carry them: the Rust SDK reuses `wire`, a client never opens a mesh
  stream, and `wire` must not depend on `raft`. The encoding in `raft` lost: `raft`
  cannot see the format version. A `raft` message travels on a one-way stream. A
  member forwards a proposal as one two-way stream of `Class::Command`: the stream
  carries one proposal, the leader writes one answer on its reply half, and then both
  halves end. So a proposal and its answers name no sender and carry no request number
  (#779): a request number makes the node that asks keep and remove open requests and
  handle a late answer (CLOCK WIRE), and HUB WIRE already answers on the stream that
  asks. A node with a ticket opens a join stream as `Class::Complete`, and its reply
  half carries one `Unjoined`, or one `Joined` and then its parts (`laptop.architect`,
  2026-10-19T11:58:22Z, item 3 of
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6266662803). A stream
  breaks the protocol when a message is the byte form of no message, when a one-way
  stream carries a proposal, a join, or an answer, when a two-way stream does not start
  with a proposal or a join, or when the half that the receiver reads carries a second
  message, other than the `Founding` parts after `Joined`. The receiver stops the stream
  with code 2 (`wire::header::MALFORMED`), the code that HUB WIRE gives for a
  broken protocol rule (decided by the architect, 2026-10-07T08:15:18Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025, and
  2026-10-07T09:42:17Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6035294122, point 3).
  A second message comes after the answer, and a reset takes back an answer that the
  peer does not have yet. So there the receiver stops only the half that it reads. At
  each other break of a two-way stream, it stops the half that it reads and resets its
  reply half, each with code 2: a two-way stream that ends with no message is such a
  break (approved by the architect, 2026-10-07T12:52:00Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038303084). A message
  that the group refuses (MESH DRIVER) changes nothing, and the receiver stops the
  stream with code 16, the first code of the mesh protocol (PROTOCOL HEADER), and resets
  a reply half with the same code. A request from a node that a committed configuration
  removed, when `Start.voters` or a committed `Voters` entry held it (RAFT VOTERS), gets
  code 17 instead (`Error::Removed`). A `Change::Card` over the bound of MESH DRIVER
  gets code 18 instead (`Error::Soon`), and the peer gives `Error::Soon` and does not
  forward it again. A join that the leader refuses as `Error::Contested` gets code 19
  instead, and the peer gives `Error::Contested` and does not forward it again
  (decided by the architect, 2026-10-18T22:14:01Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6255804864). A group
  that stopped gives code 16 on a one-way stream. On a stream that goes both ways it
  gives no mesh code: it can stop in the write of the entry, which then applies after a
  new open. A `raft` message that finds no block in the pool
  is not a refusal: the receiver drops it, the stream goes on, and `raft` sends it
  again. The receiver holds no block while the group writes the entry: it drops the
  block of the proposal before it gives the change to the group, and takes the block of
  the answer after the answer. With no block for the answer, the peer gets no answer:
  the group can hold the entry of the proposal. The reply half ends with no answer and
  no mesh code. A reply half that ends with no answer and with no code 2, 16, 18, or 19
  says nothing about the change, and the peer forwards it again. Lost: the block of the
  answer first, because a block held while the group writes can take the room that the
  write needs, and only the end of the write frees it (decided by the architect,
  2026-10-07T13:07:00Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823). The
  sentences on a group that stopped and on an answer with no block are from a later
  ruling. Lost there: code 16 that says nothing about the change after a stop, because
  code 16 carries no cause, so the peer cannot tell a stop from a refusal (decided by
  the architect, 2026-10-07T14:17:49Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6039908458).
  Supersedes, for a stream that goes both ways, point 2 of
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501, and the
  sentence "the group took the proposal" of
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823. The
  receiver does not check the class of a stream: the class sets only the priority of the
  sender (approved by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A
  message has one byte form, and a decode takes nothing else. The log (MESH LOG) and the
  messages share the byte form of an entry. Decided by `consensus`, approved by the
  coordinator (#471). `mesh::testing::round_trip_change`, behind the `sim` feature,
  gives the fuzz target `mesh_change` the decode and encode of a change record; no
  change type is public (decided by the architect, 2026-10-07T11:17:12Z:
  https://github.com/synnaxlabs/foundation/issues/1339#issuecomment-6036785855).
  `mesh::testing::round_trip_message` and `round_trip_entries` give the fuzz targets
  `mesh_message` and `mesh_entries` the decode and encode of a message and of entries
  one after another, in the same way (approved by the architect, 2026-10-08T01:06:45Z:
  https://github.com/synnaxlabs/foundation/issues/1470#issuecomment-6050048371). The
  module `change` holds the change records and their byte forms (`Change`, `Join`,
  `Malformed`, `Unknown`). The module `region` holds the state that they move (`State`,
  `Request`, `Refused`, `Unfit`). One module for both lost: `region::Unknown`, a change
  of no known kind, was not clear next to `region::Refused::Unknown`, a ticket that is
  not recorded (decided by the architect, 2026-10-07T16:24:54Z:
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6042136383).
  Each member takes a message of `message::BYTES_MIN` bytes: an `Append` with no proof
  and no link, of one entry that holds a record of `change::BYTES_MAX` bytes, the
  largest record of any kind. `mesh` gives `raft` the `Config::fits` "the byte form is
  at most `BYTES_MIN`", so a member never needs the limit of each peer. The decode of an
  `Append` refuses it when an entry is larger than one that holds a record of
  `BYTES_MAX` bytes, so no node takes an entry that no `Append` fits. The log holds
  only entries that the node took or proposed, so its decode has no such bound. The
  bound of the decode of an `Append`: `laptop.architect`, 2026-10-12T19:45:38Z
  (https://github.com/synnaxlabs/foundation/pull/2477#issuecomment-6131426790).
  `Mesh::open` refuses a node that takes less than `BYTES_MIN` (`Error::Limit`), and
  `propose_voters` refuses a set of more than 127 voters, so that the proof of a message
  with no link fits. Lost: `mesh` cuts a large message into parts and the receiver
  joins them, because nothing bounds what the receiver holds before it decodes.
  Decided by `laptop.architect`, 2026-10-12T18:29:52Z
  (https://github.com/synnaxlabs/foundation/issues/1361#issuecomment-6130215772).
  The check reads the size of the largest block of the pool, not its room: a pool whose
  budget is near that block can stall the mesh with no error (#2784). Lost: a bound on
  the budget, which refuses some pools that work and passes some that stall. Decided by
  `laptop.architect`, 2026-10-16T00:21:57Z
  (https://github.com/synnaxlabs/foundation/pull/2781#issuecomment-6205117345).
  The founding of a join answer is the one case of parts: `Joined` gives its length,
  which `FOUNDING_BYTES_MAX` bounds before the receiver reads a part, and each
  `Founding` part fits one message and the largest block of the pool (MEMBER RECORD;
  `laptop.architect`, 2026-10-19T00:03:42Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6256819172).
  The decode of the proof of a message refuses more than 254 grants, the two sets of a
  joint configuration of 127 voters each, so that each proof that `raft` keeps fits a
  message with no entry and no link. Decided by `laptop.architect`, 2026-10-12T20:07:11Z
  (https://github.com/synnaxlabs/foundation/pull/2477#issuecomment-6131749653), and the
  place of the bound 2026-10-12T20:18:47Z
  (https://github.com/synnaxlabs/foundation/pull/2477#issuecomment-6131920823).
  `Mesh::start` refuses a set of more than 127 voters with `Error::Voters`, so that each
  proof that the node makes also fits a message with no entry and no link. Decided by
  `laptop.architect`, 2026-10-12T20:38:56Z
  (https://github.com/synnaxlabs/foundation/pull/2477#issuecomment-6132216407).
  Supersedes point 3 of
  https://github.com/synnaxlabs/foundation/pull/2477#issuecomment-6131920823 for the
  founding set.
  The decode of a `Data::Voters` entry does not bound its set by `VOTERS_MAX`: only
  `propose_voters` of the leader makes one, and `raft` trusts its voters
  (`docs/security.md`). Decided by `laptop.architect`, 2026-10-12T23:42:08Z
  (https://github.com/synnaxlabs/foundation/issues/1361#issuecomment-6134741309).
