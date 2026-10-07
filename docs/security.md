# Security

The threat model of Foundation. The red-team sessions own this file and update it when
a surface lands. `docs/decisions.md` wins where they differ. A defect that an
attacker can use is a GitHub issue with the `security` label and a failing test.

## What we protect

- **Commands.** A wrong, forged, late, or replayed command moves a real machine. This
  is the worst case. A command expires at a deadline and is never replayed after an
  outage (D2).
- **Telemetry in motion and in the buffer.** Its loss, change, or disclosure, and the
  truth of its timestamps.
- **The spec.** It decides who may do what, and where each connector and index runs.
- **The audit record.** It names the subject and the forwarding node of each action.
- **Secrets.** Device and store credentials, join tickets, and each node's Ed25519
  private key and X25519 seal key (S8).
- **Availability.** A node that stops also stops the data and control paths through
  it.

## Who attacks

| Attacker | Can | Cannot |
| --- | --- | --- |
| Network peer | Reach a node's UDP and TCP port; read, drop, change, replay, and delay packets | Hold a node key or a subject key |
| Relay | Drop or delay what it carries | Read or change it: it sees only ciphertext |
| Unknown client | Open a TLS session with no certificate (`Peer::Client`) | Sign a hello for a subject in the spec |
| Subject | Sign hellos and session opens with a key in the spec | Go past its `access` allows |
| Member node | Act as itself and as the connectors placed on it; read and change the traffic of subjects connected through it; use their open sessions until the hello expires; drop or delay what it forwards | Hold another node's key; change the spec |
| Home of an index | Write any data into the index, run its gate, lie to its readers | Act outside its placement |
| Voter | Vote and stall its region; break `raft` safety (Node to node) | Forge a spec change or move a home outside placement |
| Time source | Shift the clocks that follow it, within what the estimator accepts | |
| Device | Send any bytes to a connector | Reach the core except through `hub` |
| Local user | Read and write the node's files, and so hold its keys and cached secrets and become that member node | Read memory of the process |
| Agent host | Use the key of the agent's subject, which the MCP process holds | Go past that subject's allows |
| Dependency | Ship hostile or defective code in a crate we build | |

The reach of each node role is from BQ12. Placement is the trust decision: the home
of an index is the authority for it.

Accepted in v1 (BQ12): no end-to-end integrity of frames, so a member node can change
what it forwards, commands included. Out of scope: a hostile operating system or
hardware.

## Trust boundaries and their state

Each line names the check that holds the boundary, the crate that owns it, and its
state on `main`.

### Network to `transport`

- Every carrier but the diode runs TLS 1.3 only, with ALPN `foundation/1`, no
  resumption, and no 0-RTT (NODE KEY TLS). A peer is the Ed25519 key in its leaf
  certificate. Names, dates, and the issuer are not checked. Landed in `tls`; the
  carriers are not built (#55, #68).
- TLS ends at the far node. A relay carries ciphertext and admits only known keys
  (R5, BQ12).
- The diode carrier is UDP with Noise K and no TLS. Commands, Raft, and clock exchange
  cannot cross it. Not built.
- `transport` accepts every Ed25519 key that is not of small order, and every client
  with no certificate. Admission is the caller's job. Until `node` admits a peer, the
  peer must not make the node hold memory or do work out of proportion to the bytes
  it sent.
- A message on a stream is a length and then bytes. The length is the peer's choice,
  up to `message_bytes_max`.
- `types::hash` maps hash with no key (R16-7), so a peer that chooses keys freely can
  make them collide. A QUIC stream ID is dense and bounded, so the stream maps of
  `transport` hold. A map keyed by a value a peer chooses freely needs the keyed
  hasher of R16-7, which is not built.
- A key of small order needs no private key. `types::node::PublicKey::new` refuses
  each one, so each check that takes a `PublicKey` has it (NODE KEY TLS). Landed in
  `types`; the TLS check uses it.
- Each shard signs stateless resets with one key for the node, and a connection ID
  names its shard in the first byte (ONE PORT PER NODE). A shard that gets a
  datagram with a short header for a connection of another shard signs a valid
  reset for it. The router is not built. #77 asks that it hands such a datagram
  only to the shard that the first byte names, and drops one that names no shard.
- Open: #228 (a length prefix holds a whole block of the shard's pool before a body
  byte arrives). A connection now holds at most its receive budget (#467), and a
  size takes the budget of a size with no block in use (#270). A stream on another
  connection reads while one connection holds its budget (RECV WAITS). Still open:
  many connections before admission (#563).
- Open: #607 (a stranger keeps the ID from a failed dial and makes the node send a
  reset to each address it spoofs, with no limit), #620 (a stop after the peer's
  reset gives the peer the stream's window twice, so a peer grows the connection's
  receive memory with no bound).
- Fixed: #299 (a peer made the node hold certificates that are not valid for a
  session). A chain is one certificate of at most 1 KiB. #298 (datagrams that are
  not valid, from one address, stopped every stateless reset; a small datagram of an
  unknown version got a reply).
- Not decided: a limit on handshakes before admission. Each one costs the node a key
  exchange and one signature, and one signature check more when the peer sends a
  certificate. With no limit, each spoofed Initial holds about 46 KB until the idle
  timeout (#563).

### `transport` to protocols

- The first message of a stream, and each datagram, starts with a `wire` header
  (PROTOCOL HEADER). `node` stops a stream whose header is not valid, and drops and
  counts such a datagram. A client opens only hub streams; `node` refuses the other
  protocols from a client. `wire::header` landed; the dispatch table in `node` is not
  built.

### Subject to owner

- A remote subject signs a short-lived hello and each session open. The gateway
  forwards the signatures. The owner checks them against the subject's keys in the
  spec (BQ12). A node acts only as itself or as a connector placed on it. Not built
  (`access`, `hub`).
- To attack when it lands: a replayed hello, a hello for another gateway, a session
  open with no fresh signature, a forwarding node that swaps the subject, a command
  after its deadline or replayed after an outage, and any read or write path that does
  not reach `access`, and a subject key of small order (the check must take
  `PublicKey`).

### Agent host and CLI to `ops`

- Caller to `ops`: the CLI and the MCP surface have only `version` and `docs`
  today. #353: `call` accepts a list as arguments, but the schema says `object`; a
  text error prints the caller's text raw, so a new line forges a `fix:` line. It
  does not have the `security` label while no operation takes real arguments.
- Mesh to agent, to attack when `ops` gets real operations: text from a spec file
  or a peer that reaches a tool description or a tool result, as an instruction to
  the agent.

### Node to node

- Node-to-node traffic is authorized by role (BQ12). A new node joins only with a
  signed ticket, and voters record membership (BQ11a). Every node checks each `Join`
  change at apply: the card's signature, a name and status channel names that are not
  reserved, under the region, not too long, and not held by a member, a key that is not
  yet a member, status keys that no member holds, and the ticket's admission, scope,
  uses, and expiry. A status holds at most 64 entries, so decode refuses more, and
  checks no signature. So a forged card or a body that does not decode in a committed
  entry is a refused change, not a stopped group. The proposing voter and the join
  answer are not built (#336).
- `apply` signs the plan hash, and every node checks every change record (BQ12). So
  a voter that lies can stall its region, and cannot change access, keys, or
  placement. Not built (`spec`).
- `raft` does not check the sender of a request, by decision: the caller
  authenticates the sender and decides which nodes may send (RAFT SURFACE). Not
  built (`mesh`). Before it acts, `raft` checks the index a heartbeat or an append
  answer names, the order of an append's entries, and that no entry is above the
  append's term. A node that a change removed and that missed its release can win
  an election once no voter has a lease, and lead until it commits the leave
  (#483).
- `raft` drops a reply from a node that is not a voter, unless a change removed the node
  and `raft` still sends to it (#352). It takes a higher term only with a proof that a
  quorum of its configuration granted the sender, in every message but a `PreVote` and a
  granted `PreVoteReply` (RAFT SURFACE, #750). A refusal of a lower term carries the
  proof of the refuser's term, so a node that is behind catches up.
- A node that may send to a group and lies could stop the group for good with one
  message in term `u64::MAX`. Now that message needs a quorum of grants (#750). `mesh`
  also admits a `raft` request only from a voter of the newest configuration (RAFT
  VOTERS, #654), and `raft` drops a reply from any other node. Not built (`mesh`). A
  voter that lies can still break safety, because a false `AppendReply` counts as held,
  so `raft` trusts its voters (RAFT SURFACE, #352 item 2). A signed `AppendReply` is
  #882.
- A voter that does not lead cannot make a node follow it: a heartbeat or an
  `Append` of a higher term, or of a term whose leader the node does not know yet,
  needs a quorum of votes for the sender, else `Error::Unproven` and nothing changes.
  A second leader of a term whose leader it knows is `Error::SecondLeader`.
  `raft` counts the keys of a proof, and `mesh::grant` checks each signature
  against the voter's public key. Until the driver (#471) runs that check before
  `step`, a voter can forge the keys. `raft/tests/it/hostile.rs` pins the refusal.
- A voter that was down through a configuration change holds the old configuration
  and refuses a leader it cannot prove. It rejoins at the next election whose grants
  are a quorum of what it holds. When a second node fails before that, the group
  waits for an operator: wipe the voter's state and start it with no configuration.
  The chain of proofs over configuration entries closes it (#881, a release
  blocker). `raft/tests/it/behind.rs` pins both.
- The joint quorum math of `raft::Voters` held against a direct count (the run is
  in #352). Voters do not change through the log yet (#193); attack that when it
  lands.
- A disk that lost entries it synced can break Raft safety: the leader still counts
  them toward a commit, and the node can grant a vote to a candidate that lacks a
  committed entry. `raft` does not find the loss: the disk owns durability
  (`env::files`, RAFT DURABILITY, #352 item 3). The node gets `Error::IndexPastLog`
  only once the leader's commit passes its last entry. Not built (#648): the node
  shows the error in its status.
- The `clock`, `replica`, and `blob` protocols are not built. To attack when they
  land: who may be a time source, and a binary that a peer serves under a hash it
  does not match (C9d).

### Time source to `estimate`

- `estimate` combines one bound per source and does not know what a source is
  (ESTIMATE COMBINE). A known result holds the truth when more than half of the bounds
  that vote hold it, whatever the others are. So a small bound from a lying minority
  cannot steer it off the truth (the attack of R6, #344). A lying majority can. So can
  one known bound beside unknown bounds alone, because an unknown bound does not vote
  beside a known one.

### Files to the spec

- HCL text becomes a `Document` (`config-hcl`), and a `Document` has one canonical
  encoding (`document`). Both readers bound nesting at 64 levels.
  `config_hcl::write` gives text that reads back as an equal `Document`. Fuzzed:
  `config_hcl_read`, `config_hcl_update`, `config_hcl_write`,
  `document_encoding`. Fixed: #446 (`update` put a new block after a kept block
  it must come before); the `block_before_kept` inputs hold it.
- A person or an agent reviews the files and the plan before `apply` (K3). Text
  that shows one thing and reads as another defeats that review. Questions for a
  decision, with no `security` label yet: #360 (a lone `\r` in a comment,
  bidirectional controls in the reader and in written text, keys compared by
  bytes, a heredoc that closes on its marker followed by U+00A0).
- #400 (`security`): `a = 1` and `a` U+200D `= 2` read as two keys, and the writer
  writes the joiner raw, so a file and a diff show one key twice (HCL does the
  same). A `Name` is ASCII (A3), so the reach is a key no schema checks: an object
  key in a free-form map, and an attribute key until `config` refuses an unknown
  one. Proposed: a `Diagnostic` from `config-hcl` for an identifier or an object
  key with a `Default_Ignorable_Code_Point`, so the reader still reads as HCL does
  (HCL IDENTIFIERS) and the diagnostic stops the `apply`.
- Config names secrets and never holds their values (K4).

### Disk to `buffer`

- The disk can tear, cut, flip, or zero bytes, and can hold records from an older lap
  of the ring. A chained CRC32C finds these. It does not stop a local user who writes
  the file: the CRC is not a secret, and a header block has no tie to its ring.
- The engine landed (#161): `Buffer::open` reads the header blocks and walks the
  ring. #234 and #300 are robustness defects of this boundary, with fixes in
  review (#356, #348). They do not have the `security` label: each needs a writer
  of the file, or, for the small body of #300, a `Layout` from the node's own
  config (a new ring with a body of 4 to 54 bytes stops the node at its first
  `append`).
- Fuzzed: `buffer_open`, which opens the ring and reads each path back. Open on `main`:
  #392 (three ways a ring loses data it reported durable or cannot open), #566 (a write
  of a dead process can land on a ring that a new process opened), #572 (`append` takes
  a record over the pool's largest block, and then each open fails), #657 (an open
  reports durable the records a killed process never synced). Fixed: #553 (a power cut
  after the first open lost the new ring: its directory was not synced in its parent),
  #393 (two CRC-valid fields stopped the node at open); the `area` and `below_tail`
  inputs hold both.

### Device to connector

- Each protocol parser reads bytes from a device. A connector reaches the core only
  through `hub`. Not built. Each parser gets a fuzz target when it lands.

### Encoded series

- `codec::validate`, `codec::decode`, and `codec::Decoder` read series from peers
  and from disk. A series cannot make `decode` or `Decoder` write outside `out`.
  Fuzzed: `codec_series`, `codec_encoder`.

## Secrets

- `types::node::PrivateKey` writes `PrivateKey(..)` in `Debug`, and has no `Display`
  and no equality. The TLS configs do not write key bytes in `Debug`.
- Open hardening: `PrivateKey` is `Clone` with a public field, and neither it nor the
  PKCS#8 copy in `tls` is cleared when dropped.
- Node key material is on the node's local disk. A local user who reads it is that
  node.
- `ctx.secret(name)` is the only path to a secret value (SECRET STORES AS ADAPTERS).
  The built-in store seals each value to the X25519 seal key of each node that may
  use it (BQ16, S8). The other adapters (an environment variable or a file, and the
  external stores) do not. An external adapter authenticates with the node key, and
  may cache values sealed to it, which delays revocation.
- Sealing is HPKE base mode, so it does not prove who sealed. The signed record
  does: the caller seals and signs the `secret set` request with the `secret`
  action, and every node checks that record as it checks a spec change (BQ12). So a
  lying voter cannot write a ciphertext record. Not built (`mesh`). A node of the
  secret's placement re-seals on a key rotation (BQ16), so it can also re-seal a
  different value. Accepted: it already holds the value.
- Each sealed value has a version per name, bound into the associated data, so a
  writer cannot give an old value a new version. `secret::store::Sealed` refuses a
  value that does not open at its version. The order of versions has one check, on
  the record: every node takes a write only at the newest version plus one, so a
  replayed record and a jump to the last version are refused. A re-seal keeps the
  version and goes only to a node of the placement, so a node that leaves the
  placement keeps no copy in region state. The newest version of a name outlives a
  delete and the secret's removal. Not built (`mesh`).
- A join ticket is a secret (BQ11a).
- Rule for every crate: no secret value in a log, a status channel, an error, or
  plan output.

## Supply chain

- Each third-party crate needs a person's approval (`docs/dependencies.md`).
  `cargo deny check advisories bans licenses sources` runs in CI on each change to a
  manifest. aws-lc-rs is the only crypto provider, with one recorded exception.
- `unsafe` is denied in the workspace. The crates that allow it (`block`, `ring`,
  `counting`) run under Miri in CI.
- The `fuzz/` crate has its own lock file, which `cargo deny` does not read (#252).
- A node fetches the signed binary of a release by hash from a nearby peer (C9d). The
  signing key and its check are not built.

## Fuzz targets

The rule is one target for each decoder of outside input
(`docs/claude/testing.md`). An encoder or a writer also gets a target when a
decoder must read its output back (`codec_encoder`, `config_hcl_write`). Inputs are
in `oracles/fuzz/<target>/`. The CI job is #252.

| Target | Surface | Checks besides "no panic" |
| --- | --- | --- |
| `wire_header` | `wire::header::decode` | Encodes to the same bytes |
| `wire_clock` | `wire::clock::decode` | Encodes to the same bytes |
| `wire_hub_home` | `wire::hub::Home::decode`, `Open::encode`, `Credit::encode`, `keys::encode` | Each message encodes to the same bytes; each valid message made from the input decodes to itself |
| `wire_hub_reader` | `wire::hub::Reader::decode`, `Reply::encode`, `ends::encode` | Each message encodes to the same bytes; the body is where `Reader::body` says; each valid message made from the input decodes to itself |
| `mesh_change` | `mesh::region::Change::decode`, and `Card::decode` and `Status::decode` through a `Join`, by `mesh::testing::round_trip_change` | Encodes to the same bytes |
| `codec_series` | `codec::validate`, `codec::decode`, `codec::Decoder` | All give one result |
| `codec_encoder` | `codec::Encoder` | Its output is valid and decodes unchanged |
| `document_encoding` | `document::encoding::decode` | Encodes to the same bytes |
| `spec_definition` | `spec::definition::Definition::decode` | Encodes to the same bytes |
| `config_hcl_read` | `config_hcl::read` | The encoding decodes to an equal document |
| `config_hcl_update` | `config_hcl::update` | Its text reads as the document; an update to its own document keeps each byte; an unread text gives the problems of `read` |
| `config_hcl_write` | `config_hcl::write` | Its text reads back as an equal document |
| `connector_modbus_rtu` | `connector_modbus::rtu::decode_request`, `decode_reply`, `pdu::Request::decode`, `Request::decode_reply` | A request reads back unchanged; a reply has the asked count |
| `connector_modbus_tcp` | `connector_modbus::tcp::decode`, `pdu::Request::decode`, `decode_reply` | A request reads back unchanged; a reply has the asked count |
| `ops_mcp` | `foundation mcp`, through `ops::cli` | No error, and at most one reply for each line |
| `types_name` | `Name` | Prints as the text it was read from |
| `types_selector` | `Pattern`, `Selector` | Agree with a second matcher |
| `types_stamp` | `Stamp` | Printed text reads back to the same value |
| `types_span` | `Span` | Printed text reads back to the same value |
| `types_range` | `Range` | Printed text reads back to the same value |
| `types_byte_size` | `byte::Size` | Printed text reads back to the same value |
| `types_channel` | `channel::Key` | Printed text reads back to the same key |
| `types_frame_ends` | `frame::Layout::from_ends`, `frame::check`, `frame::split` | Refuses exactly the ends that break a rule, with an error that names a broken rule; the layout is the one that `Layout::new` gives for the lengths; a frame drafted from the ends has them, and `split` cuts its series at them; `check` refuses exactly the ends that do not fit a body whose length the input gives, and `split` cuts a body that `check` took at them. Not reached: the panics of `split`, a body over 64 KiB |
| `buffer_open` | `Buffer::open` and `Buffer::read` on an edited ring | An `Err`, or a commit survives a reopen; a read gives each path as the doc of `Buffer::read` says, up to the tail, the same in one read, in steps, from inside an entry or a gap, and after a reopen. Not reached: a table over one block, a pool with no block, a read before a commit ends |
| `secret_sealed` | `secret::store::Sealed::put` | Takes only the one real sealed value; refuses any other bytes, name, or version; a refused `put` leaves the store as it was |

No target yet, because the decoder is private or not built: `transport::message`
and `tls` (#55), the QUIC hello (`transport::quic::hello::Hello::decode`), `raft`
messages, `mesh::Member::decode` (the join answer of #336 adds its target), `spec`
tree chunks (#64), `types::time::Rate`, and each connector's protocol parser.
