# Security

The threat model of Foundation. The red-team sessions own this file and update it when
a surface lands. `docs/decisions/` wins where they differ. A defect that an
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
  make them collide. A peer must use its QUIC stream IDs in order, and `streams_max`
  limits how many are open in each session, so a lookup in a stream map of one session
  costs at most that many compares. Open: #1506 (the map of reads that wait for a block
  holds the streams of each session of a carrier, so that bound does not hold for it). A
  map whose keys a peer picks is a `BTreeMap`, unless the node limits those keys to a
  small count, as `streams_max` does for the streams of one session (R16-7).
- A key of small order needs no private key. `types::ed25519::PublicKey::new`
  refuses each one, so each check that takes a `PublicKey` has it (NODE KEY TLS).
  Landed in `types`; the TLS check uses it.
- Each shard signs stateless resets with one key for the node, and a connection ID
  names its shard in the first byte (ONE PORT PER NODE). A shard that gets a
  datagram with a short header for a connection of another shard signs a valid
  reset for it. The router is not built. #77 asks that it hands such a datagram
  only to the shard that the first byte names, and drops one that names no shard.
- #228 (a length prefix held a whole block of the shard's pool before a body byte
  arrived): a prefix now takes no block. A reader keeps the bytes of a message that
  waits for a block in one heap buffer, outside the shard's pool, and the receive
  budget counts them (#1456). A connection holds at most its receive budget (#467),
  and a size takes the budget of a size with no block in use (#270). A stream on
  another connection reads while one connection holds its budget (RECV WAITS). Still
  open: many connections before admission (#563), which also bounds the sum of
  those heap buffers. Open: #1482 (an out-of-order frame that noq-proto's assembler
  holds pins its whole receive allocation, up to 64 KiB, so a peer that sends such
  frames holds more memory than the budget counts).
- Each shard's endpoint sends at most one stateless reset to each address in each
  20 ms window (#607). An address is an IPv4 address or the first 64 bits of an IPv6
  address. Addresses hash into 65,536 buckets with a key from `Entropy`. Residual risk
  (#655): a stranger with an ID from a failed dial still gets 50 resets a second for
  each shard (N x 50 for a node of N shards) sent to each victim address, each smaller
  than the datagram that caused it. A sender that can use the peer's address (it
  spoofs it, shares the peer's NAT, or is in the peer's IPv6 /64) takes that peer's
  resets, and the peer then ends at its idle timeout; peers that share an address
  share 50 resets a second on each shard. One ID is enough, and a peer whose dial
  completes gets new IDs with no limit: noq-proto issues a new ID each time the peer
  retires one.
- Open: #620 (a stop after the peer's reset gives the peer the stream's window twice,
  so a peer grows the connection's receive memory with no bound).
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
  protocols from a client. A node with a region gives each `Mesh` stream of a peer
  that proved a node key to `Mesh::serve`, which checks each message (NODE MESH), and
  rejects a client's. It gives a `Hub` stream to the hub only when a member of its
  region has the peer's public key, once, at the header (NODE PORT); it rejects a
  client's until #1744. `node` stops and resets each other stream until its protocol
  has a server. It reads no datagram yet (#1661), and admits every peer (#1628).

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
  placement. Not built (`spec`). Until #1213, a `Change::Spec` has no signature, and
  the leader proposes one that any voter forwards with the holders it names. So any
  voter can move the spec pointer to any root, with chunks that no voter holds.
- `raft` does not check the sender of a request, by decision: the caller authenticates
  the sender and decides which nodes may send (RAFT SURFACE). `Mesh::receive` refuses a
  message whose sender is not the peer that holds the stream (`Error::Spoofed`). `node`
  serves mesh streams when it has a region (NODE MESH). Before it acts, `raft` checks
  the index a heartbeat or an append answer names, the order of an append's entries, and
  that no entry is above the append's term. A node that a change removed and that missed
  its release campaigns; a voter whose log holds the leave refuses the request, with
  `removed` once the leave commits, and the node stops (#1105). A voter whose log lacks
  the leave entry admits the request until #1107, so in `raft` alone such a node can win
  an election once no voter has a lease, and lead until it commits the leave.
- `raft` drops a reply from a node that is not a voter, unless a change removed the node
  and `raft` still sends to it (#352). It takes a higher term only with a proof that a
  quorum of its configuration granted the sender, in every message but a `PreVote` and a
  granted `PreVoteReply` (RAFT SURFACE, #750). A refusal of a lower term carries the
  proof of the refuser's term, so a node that is behind catches up.
- A node that may send to a group and lies could stop the group for good with one
  message in term `u64::MAX`. Now that message needs a quorum of grants of a
  configuration the node holds, or of one that a chain of signed configuration
  entries proves (#750, #881). A link carries only its leader's signature and the
  votes of its term, so a voter that led a term at or above the node's committed one
  (after a restart, the term at its applied index, since `Hard` holds no commit index;
  architect, https://github.com/synnaxlabs/foundation/pull/1682#issuecomment-6050014758)
  can sign a configuration entry it never wrote, to a configuration of itself alone, put
  it in a chain, prove any term with its own grant, and so stop the group for good.
  `raft` trusts its voters until #882, which gives a link the signed acks of a quorum;
  `crates/raft/tests/it/hostile.rs` pins the gap (architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6043096423). `mesh`
  also admits a `raft` request only from a voter of the newest configuration (RAFT
  VOTERS, #654), and `raft` drops a reply from any other node. `Mesh::receive`
  refuses such a request (`Error::NotVoter`). It answers `removed` (`Error::Removed`,
  code 17) only to a sender that a committed configuration removed, so a stranger
  cannot learn from the answer which nodes the log held, and a sender stops its group
  only on that answer from a voter of its own configuration (#1105). `node` serves
  mesh streams when it has a region (NODE MESH).
  A voter that lies can still break safety, because a false `AppendReply` counts as
  held, so `raft` trusts its voters (RAFT SURFACE, #352 item 2). A join that a
  voter that lies writes gives its node the key it names (MESH DRIVER). A signed
  `AppendReply` is #882.
- A voter that does not lead cannot make a node follow it: a heartbeat or an
  `Append` of a higher term, or of a term whose leader the node does not know yet,
  needs a quorum of votes for the sender, else `Error::Unproven` and nothing changes.
  A second leader of a term whose leader it knows is `Error::SecondLeader`. The
  exception is a voter that led a term at or above the node's committed one: it can
  forge a link until #882 (the bullet above). `raft` counts the keys of a proof, and
  `mesh::claim` checks each signature against the voter's public key.
  `Mesh::receive` runs that check before `step`, and `Mesh::serve` runs it for each
  `raft` message of a one-way stream. `node` serves mesh streams when it has a
  region (NODE MESH).
  `raft/tests/it/hostile.rs` pins the refusal and the gap.
- A voter that was down through a configuration change holds the old configuration.
  The new leader's message carries the chain of configuration entries below its
  term, each with the votes and the signature of the leader that wrote it
  (`raft::Change`). The voter reads the chain up to the entry whose configuration
  the leader's votes are a quorum of, checks each signature it reads, and follows
  the leader; it keeps nothing from the chain. A forged link, or one whose votes are
  no quorum of the configuration that elected its leader (the last link read of a
  lower term, else the voter's last committed configuration of a lower term), is
  refused, and the voter does not change (architect, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579).
  `raft/tests/it/behind.rs` pins that the voter follows the leader, `mesh::claim` pins a
  forged link, and the `chain` tests in `crates/raft/src/machine.rs` pin the quorum
  rule. The chain does not cover a leader that the missed change made a voter (#1096),
  and it cannot prove a term that no configuration entry stands behind: a node that a
  leave removed can reach such a term, and a change that adds it back then stalls the
  group (#1485).
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
  it must come before); the `block_before_kept` inputs hold it. `config::check`
  reads the documents into definitions, and the influx kind reads the config of each
  `connector` block of kind `influx`. Fuzzed: `config_check`, which no input yet
  takes to the influx kind (#1817).
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
  of the ring. A chained CRC32C finds these, with two exceptions that are open on
  `main` (#1441): when the disk cuts the file to zero bytes, or when the first sector
  of each header block reads as zero, an open takes the file for a ring with no
  checkpoint, removes it, and makes a new ring with no error. The CRC does not stop a
  local user who writes the file: it is not a secret, and a header block has no tie
  to its ring.
- The engine landed (#161): `Buffer::open` reads the header blocks and walks the
  ring. #234 and #300 were robustness defects of this boundary, fixed in #356 and
  #348. They do not have the `security` label: each needed a writer of the file, or,
  for the small body of #300, a `Layout` from the node's own config (a new ring with
  a body of 4 to 54 bytes stopped the node at its first `append`).
- Fuzzed: `buffer_open`, which opens the ring and reads each path back. Its inputs
  reach a record of four blocks, an entry table of four blocks, a tail at each block
  of the area, a wrap record, a full ring, and the end of the offsets. Fixed: #392
  (three ways a ring lost data it reported durable or could not open), #566 (a write
  of a dead process could land on a ring that a new process opened), #572 (`append`
  took a record over the pool's largest block, and then each open failed), #657 (an
  open reported durable the records a killed process never synced), #553 (a power cut
  after the first open lost the new ring: its directory was not synced in its parent),
  #393 (two CRC-valid fields stopped the node at open); the `area_16` and
  `below_tail_16` inputs hold the two fields of #393.

### Device to connector

- Each protocol parser reads bytes from a device. A connector reaches the core only
  through `hub`. Not built. Each parser gets a fuzz target when it lands.

### Encoded series

- `codec::validate`, `codec::decode`, and `codec::Decoder` read series from peers
  and from disk. A series cannot make `decode` or `Decoder` write outside `out`.
  Fuzzed: `codec_series`, `codec_encoder`, `codec_string`, `codec_shape`, and
  `codec_shape_encoder`.

## Secrets

- `types::ed25519::PrivateKey` writes `PrivateKey(..)` in `Debug`, and has no `Display`
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
- A local patch of a Rust crate (`patches/`) is a path package, which `cargo deny`
  does not check against advisories. The `Advisories of each patched release` step of
  the `deny` job checks its release (#1867). The open62541 copy in `patches/open62541/`
  is C, not a crate: no check compares it with advisories until #1910.
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
| `wire_hub_home` | `wire::hub::Home::decode`, `Open::encode`, `Credit::encode`, `keys::encode` | Each message encodes to the same bytes; each event comes in the order of a session, and each refusal is one that the order or the mode of the session gives; each valid message made from the input decodes to itself |
| `wire_blob` | `wire::blob::Server::decode`, `Requester::decode`, `get::encode`, `Put::encode`, `Reply::encode` | Each message encodes to the same bytes; each body message is where `body` says and no longer than the rest of the body; each refusal is the one the state gives; each valid message made from the input decodes to itself |
| `wire_hub_reader` | `wire::hub::Reader::decode`, `Reply::encode`, `ends::encode` | Each message encodes to the same bytes; each event comes in the order of a session, and each refusal is one that the order or the mode of the session gives; the body is where `Reader::body` says; each valid message made from the input decodes to itself |
| `wire_hub_client` | `wire::hub::client::Challenge::decode`, `Signed::decode`, `Request::decode`, `Response::decode`, `Body::take`, `Body::end`, and the encoders of each message | Each message encodes to the same bytes; each decoder refuses another kind with `Error::Kind`; each body ends at its length and nowhere else, and each refusal of a body is the one that its rest gives; each valid message made from the input decodes to itself |
| `transport_hello` | `transport::fuzzing::Hello::decode`, `Hello::encode` (feature `fuzzing`) | Gives the hello, or the refusal, that a second reader of the STREAM WIRE rules gives; its encoding decodes to itself |
| `transport_certificate` | `transport::fuzzing::peer`: the client verifier and the peer of a node's server, for a dialer's chain of 0 to 3 certificates (feature `fuzzing`) | Gives the peer that a second reader of the rules gives: a client for no certificate, none for a chain of more than one or a certificate over 1024 bytes, and else none or the node whose key follows the Ed25519 key header in the certificate; a certificate that a node issues reads back to its key, and two of it are refused. Not reached: the handshake signature, which fuzzed bytes cannot make |
| `mesh_change` | `mesh::change::Change::decode`, and `Card::decode` and `Status::decode` through a `Join`, by `mesh::testing::round_trip_change` | Encodes to the same bytes |
| `mesh_message` | The decode of a mesh message, with its `raft` proof, chain, and entries, by `mesh::testing::round_trip_message` | Encodes to the same bytes |
| `mesh_entries` | The decode of `raft` entries one after another, as a mesh log record body and an append hold them, by `mesh::testing::round_trip_entries` | Encode to the same bytes |
| `mesh_log` | The decode of one mesh log record by `mesh::testing::round_trip_log_record`: the header, its version, and the hard state and entries of the body, after `seal_log_record` writes the length and both checks | Encodes to the same bytes |
| `codec_series` | `codec::validate`, `codec::decode`, `codec::Decoder` | All give one result |
| `codec_encoder` | `codec::Encoder` | Its output is valid and decodes unchanged |
| `codec_string` | `codec::Encoder`, `codec::validate`, `codec::decode` on a `String` series | Each refuses at the first sample that `str::from_utf8` refuses, and at no other |
| `codec_shape` | `codec::validate`, `codec::decode` on an array, matrix, list, `String`, or `Bytes` series | Both give one result; an array or a matrix gives the result of the series of its elements; a `String` series gives the result of a `Bytes` series or the first sample that `str::from_utf8` refuses; a valid series decodes with zeros for the padding, and encodes and decodes unchanged |
| `codec_shape_encoder` | `codec::Encoder` on an array, matrix, list, `String`, or `Bytes` series | Gives the refusal that a second reader of the raw form gives, or a valid series that decodes unchanged with zeros for the padding; an array or a matrix encodes as the series of its elements |
| `document_encoding` | `document::encoding::decode` | Encodes to the same bytes |
| `spec_definition` | `spec::definition::Definition::decode` | Encodes to the same bytes |
| `spec_tree` | `spec::tree::get`, `apply`, `diff`, and `spec::region::definitions` on chunks from a peer | `get` agrees with a whole `diff`; `apply` gives the entries with the changes; `definitions` gives the decode of the entries only when `spec::region::tree` of them has the same root |
| `spec_data_type` | `spec::data_type::DataType` | Prints as the text it was read from |
| `config_hcl_read` | `config_hcl::read` | The encoding decodes to an equal document |
| `config_hcl_update` | `config_hcl::update` | Its text reads as the document; an update to its own document keeps each byte; an unread text gives the problems of `read` |
| `config_hcl_write` | `config_hcl::write` | Its text reads back as an equal document |
| `config_plan` | `config::plan::Plan::decode` | Encodes to the same bytes |
| `config_check` | `config::check` on the documents that `config_hcl::read` reads from up to three files, with the influx kind in the kind table | The same entries for the files in either order, or problems in both; with no problem, one entry for each block, unique in any case, each policy and connector decodes to itself, and each edge of a channel names a channel entry; each problem's span is in its file, in the order of the files, then of the source; files that pass alone, with keys that differ in more than case and no subject named as a connector in any ASCII case, pass together and give the union of their entries |
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
| `types_sample` | `sample::Type` | Prints as the text it was read from |
| `types_frame_ends` | `frame::Layout::from_ends`, `frame::check`, `frame::split` | Refuses exactly the ends that break a rule, with an error that names a broken rule; the layout is the one that `Layout::new` gives for the lengths; a frame drafted from the ends has them, and `split` cuts its series at them; `check` refuses exactly the ends that do not fit a body whose length the input gives, and `split` cuts a body that `check` took at them. Not reached: the panics of `split`, a body over 64 KiB |
| `buffer_open` | `Buffer::open` and `Buffer::read` on an edited ring | An `Err`, or a commit survives a reopen; a read gives each path as the doc of `Buffer::read` says, up to the tail, the same in one read, in steps, from inside an entry or a gap, and after a reopen. Not reached: a pool with no block, a read before a commit ends |
| `secret_sealed` | `secret::store::Sealed::put` | Takes only the one real sealed value; refuses any other bytes, name, or version; a refused `put` leaves the store as it was |

No target yet, because the decoder is private, not built, not reached from a file, or
not reached from the corpus:
`transport::message` (#55), the QUIC hello
(`transport::quic::hello::Hello::decode`), `mesh::Member::decode` (the join answer of
#336 adds its target), `spec` tree chunks (#64), `types::time::Rate`, the scan of the
mesh log files and the names of their directory (`mesh::log::scan` and
`mesh::log::sequence`, #1746), each connector's protocol parser, the OPC UA binary
decoding of open62541 (`UA_decodeBinary`, #1885), and `connector::reader::read`,
`connector::http::uri`, and `connector_influx::Kind::parse`, which `config_check`
reaches only from an input with a `connector` block of kind `influx`, and no input holds
one yet (#1817).
