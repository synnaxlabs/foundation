- **MESH WIRE (#471)** `mesh` encodes what two nodes of a region say on a stream of
  `wire::Protocol::Mesh`, behind the `wire` stream header: a `raft::Message`, a proposal
  that a follower forwards to the leader, and its two answers (the position of the
  entry, or "not the leader" with the leader the receiver knows). `wire` does not carry
  them: the Rust SDK reuses `wire`, a client never opens a mesh stream, and `wire` must
  not depend on `raft`. The encoding in `raft` lost: `raft` cannot see the format
  version. A `raft` message travels on a one-way stream. A member forwards a proposal as
  one two-way stream of `Class::Command`: the stream carries one proposal, the leader
  writes one answer on its reply half, and then both halves end. So a proposal and its
  answers name no sender and carry no request number (#779): a request number makes the
  node that asks keep and remove open requests and handle a late answer (CLOCK WIRE),
  and HUB WIRE already answers on the stream that asks. A stream breaks the protocol
  when a message is the byte form of no message, when a one-way stream carries a
  proposal or an answer, when a two-way stream does not start with a proposal, or when
  it carries a second message. The receiver stops the stream with code 2
  (`wire::header::MALFORMED`), the code that HUB WIRE gives for a broken protocol rule
  (decided by the architect, 2026-10-07T08:15:18Z:
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
  code 17 instead (`Error::Removed`). A group that stopped gives code 16 on a one-way
  stream. On a stream that goes both ways it gives no mesh code: it can stop in the
  write of the entry, which then applies after a new open. A `raft` message that finds
  no block in the pool is not a refusal: the receiver drops it, the stream goes on, and
  `raft` sends it again. The receiver holds no block while the group writes the entry:
  it drops the block of the proposal before it gives the change to the group, and takes
  the block of the answer after the answer. With no block for the answer, the peer gets
  no answer: the group can hold the entry of the proposal. The reply half ends with no
  answer and no mesh code. A reply half that ends with no answer and with no code 2 or
  16 says nothing about the change, and the peer forwards it again. Lost: the block of
  the answer first, because a block held while the group writes can take the room that
  the write needs, and only the end of the write frees it (decided by the architect,
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
