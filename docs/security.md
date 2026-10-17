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
- **Secrets.** Device and store credentials, join tickets, each node's Ed25519
  private key and X25519 seal key (S8), and the first admin's private key in
  `admin.key` (NODE MESH), which may take each action on `**` in the root region
  (FIRST ADMIN).
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
| Voter | Vote and stall its region; break `raft` safety (Node to node); until #1273, commit a home that is not a member, so that each node that opens an unnamed reader of its index aborts | Forge a spec change, not built (#1213), or move a home outside placement, not built (#2287) |
| Time source | Shift the clocks that follow it, within what the estimator accepts | |
| Device | Send any bytes to a connector | Reach the core except through `hub` |
| Local user that runs the node | Read and write the node's files and memory, and so hold its keys and cached secrets and become that member node. On the node that founds the root region, read `admin.key` and so be the first admin of the region, not only that node | |
| Other local user | Read, write, or list a file or directory that the operator opens to it: one that is there with a wider mode, which `os` does not change, or one that an ACL opens: an ACL of the data directory, or the entries that a directory gives to its new files, which a file that `os::paths` writes there takes on macOS, also in place of a file that had none. With write, it can write the node's keys and so become that node. With write to the data directory before the start that founds the root region, it can write `admin.key`, which that start founds with, and so choose the first admin of the root region. With read of `admin.key`, it is the first admin | Read, write, or list another file or directory that `os` makes; read memory of the process |
| Agent host | Use the key of the agent's subject, which the MCP process holds | Go past that subject's allows |
| Dependency | Ship hostile or defective code in a crate we build | |

The reach of each node role is from BQ12. Placement is the trust decision: the home
of an index is the authority for it.

Accepted in v1 (BQ12): no end-to-end integrity of frames, so a member node can change
what it forwards, commands included. Out of scope: a hostile operating system, root, or
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
- A peer that keeps back or reorders an acknowledgment, or that stops a stream and
  never acknowledges its reset, makes a session hold at most `window_bytes` of sent
  bytes (#2361). Each held run of over 1452 bytes holds its whole block, so the
  session holds at most `window_bytes / 1453` blocks, plus two for each stream. Open:
  #2434 (a session that sends small parts of large blocks, as `hub` does, holds up to
  that many large blocks).
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
- Fixed: #2084 (a peer with no key started a handshake that it kept alive, and the
  stop of the node waited for it with no bound, because shard 0 holds `lock` until
  the transport frees the port). The drop of the transport closes each handshake in
  flight, and the transport waits at most 3 s for the closes to drain after the last
  session ended, so the round trip of a peer cannot hold the port.

### `transport` to protocols

- The first message of a stream, and each datagram, starts with a `wire` header
  (PROTOCOL HEADER). `node` stops a stream whose header is not valid, and drops and
  counts such a datagram. A client opens only hub streams; `node` refuses the other
  protocols from a client. A node with a region gives each `Mesh` stream of a peer that
  proved a node key to `Mesh::serve`, which checks each message (NODE MESH), and rejects
  a client's. It gives a node's `Hub` stream to the hub only when a member of its region
  has the peer's public key, once, at the header (NODE PORT). With a region, it gives a
  client's to the hub too, which admits the client by its hello against the access rules
  of the root region of the spec in use, and of each later spec; `node` answers each
  request that the hub verified with `ops::serve`. With no region, it rejects a
  client's. `node` stops and resets each other stream until its protocol has a server.
  It reads no datagram yet (#1661). It admits each member of the region to a session,
  and gives other peers at most 256 places by default (`node::Settings::places_max`,
  NODE PORT): one for each session of a program, and one for the key of a node. At the
  bound, the oldest place frees for the new peer, and its sessions close unless its peer
  is now a member. So a peer outside the region keeps its session until that many later
  peers took a place. A flood that takes that many places in the time that a peer needs
  to prove its right (a member, the client hello of #1744 (#2548), or the ticket of
  #1157) closes that peer's session; only a limit on handshakes before admission bounds
  that rate (#563). It rejects a stream whose header has not arrived within 10 s by
  default (`node::Settings::header_wait`).

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

- Caller to `ops`: the CLI and the MCP surface have `version`, `docs`, `plan`,
  `apply`, and `status`. The MCP tools read and write files only under their
  folders, never in the data directory (OPS OUTPUT). `tools/call` takes only an
  object as arguments (#310), and a text error escapes the caller's text (#377).
- Mesh to agent: text from a spec file or a peer that reaches a tool description or a
  tool result (`status`, `plan`), as an instruction to the agent (#2924).
- A change of each definition other than a channel needs `admin` on its key, because
  a policy governs its whole region, a subject's keys decide who acts as it, a region
  record makes a region, and a connector reaches channels by `select` (SERVE ACCESS).
  So grant `admin` only to a subject that may change access, policy, and connectors
  for the whole region. A changed channel also needs `apply` on the channel at each
  of its edges.

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
- The `clock` and `replica` protocols are not built. To attack when they land: who may
  be a time source.
- `blob::peer::serve` gives any chunk whose digest the peer names, and stores any chunk
  that hashes to its digest (BLOB PEER). It trusts no length: a head over the largest
  block of the pool stops the stream with `TOO_LARGE`, a body is counted against its
  head, and a chunk is stored only when its bytes hash to the digest, else `MISMATCH`. A
  peer can fill the disk only to the free floor of the store, plus at most one chunk for
  each put in flight; then each put of a chunk that the disk does not hold stops with
  `FULL`. No code removes a chunk (#1233), so that room stays taken, and the node's own
  puts of new chunks then give `Error::Floor` too. `serve` holds at most one chunk in
  memory for each stream. `transport` also holds each chunk whole while noq keeps a sent
  byte of it (STREAM WIRE), so a get can hold more than one chunk. The chunks that
  `transport` holds for a session have at most `window_bytes` bytes together, plus two
  chunks for each stream (#2361). Who may open a blob stream is not built: `node`
  rejects each one until it routes them from member nodes only (#1229).
  The requester is not built (#1229). To attack when it lands: a binary that a peer
  serves under a hash it does not match (C9d).

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
  `connector` block of kind `influx`. Fuzzed: `config_check`, which only the inputs
  `influx_*` take to the influx kind, through `connector::reader::read`,
  `connector::http::uri`, `secret::Source::read` (the inputs `influx_token_*`, and
  `influx_ca_*` and `influx_address_https` for `ca`), and
  `connector_influx::Kind::parse` (#1817).
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
  local user who can write the file (Who attacks): it is not a secret, and a header
  block has no tie to its ring.
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
  `below_tail_16` inputs hold the two fields of #393. The `carry` inputs hold a
  carry record (#160): one valid, one each with a count past the body, an unknown
  path byte, and a carried tail below the path's tail, one that continues the chain
  only from the second header, one inside a data record, off the way, one that only
  the older header reaches, one that passes the end of the area, and one after a
  data record longer than the window. Six more reach the rule for a path that the walk
  met: `carry_passed` gives a larger count to a path whose only entry has no samples,
  and `carry_seq`, `carry_stamp`, and `carry_more_after_carry` give another seq, another
  stamp, and a larger count after a carried tail: the open refuses each (#2796);
  `carry_met_after_carry` gives a larger count to a path whose only entry has no
  samples, then a second carry record, and the open refuses the first; and `carry_twice`
  lists one path twice in one record, which it refuses. Three hold entries in a carry
  record, and the open refuses each: `carry_entry` holds one valid entry,
  `carry_entry_cut` counts one entry that the body does not hold, and
  `carry_entries_over_max` counts 1024 entries. Four reach the count of an
  entry with no samples (#2796), and the open refuses each: `empty_count_zero` gives a
  count of 0, `empty_presence` a presence byte of 2, `empty_counted` a count that is not
  the walked one on a path that the walk met, and `empty_crowded` an entry with no
  samples after a count of `u64::MAX` at its seq. The `body_over_max` input holds a data
  record that follows the chain with a body over `body_max`.

### Device to connector

- Each protocol parser reads bytes from a device. A connector reaches the core only
  through `hub`. Not built. Each parser gets a fuzz target when it lands.
- `connector-opcua` decodes OPC UA with the C code of open62541. Fuzzed:
  `connector_opcua_decode`. Its chunk processing is not fuzzed yet (#1990).
- The random generator of the open62541 copy is PCG32, which a peer can predict. The
  copy takes the nonces of the security policy None and the session token of its server
  from it. OPEN62541 SOURCE bars it for each nonce, key, and session token of Foundation
  code, and states what a security policy that encrypts, or an OPC UA server of
  Foundation, changes first. Neither is built.

### Encoded series

- `codec::validate`, `codec::decode`, and `codec::Decoder` read series from peers
  and from disk. A series cannot make `decode` or `Decoder` write outside `out`.
  Fuzzed: `codec_series`, `codec_encoder`, `codec_string`, `codec_shape`, and
  `codec_shape_encoder`.

## Secrets

- `types::ed25519::PrivateKey` writes `PrivateKey(..)` in `Debug`, and has no `Display`
  and no equality. The TLS configs do not write key bytes in `Debug`.
- Open hardening: `PrivateKey` is `Clone` with a public field, and neither it nor the
  PKCS#8 copy in `tls` is cleared when dropped. `node` does not clear the copies of
  `node.key` that it reads and writes (the encoded bytes, the pool block of each file
  call, and `Held::Written`), so the seal key that `secret::seal::Opener` clears is
  still in them. It does not clear the same copies of `admin.key`, or the first
  admin's `PrivateKey`, which it drops once the founding has its public half.
- Node key material is on the node's local disk. A local user who can read it (Who
  attacks) is that node. The node that founds the root region keeps the first admin's
  private key in `admin.key` in its data directory (NODE MESH), at mode 0600 when `os`
  makes it, so no other local user can read it unless the operator opens it (Who
  attacks). A local user who can read that file is the first admin.
- `ctx.secret(source)` is the only path to a secret value (SECRET SOURCES, SECRET
  STORES AS ADAPTERS).
  The built-in store seals each value to the X25519 seal key of each node that may
  use it (BQ16, S8). The external stores do not. Neither does `env()` or `file()`
  (SECRET SOURCES). An external adapter authenticates with the node key, and
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
- A person who can apply a spec can make the node read any variable of its
  environment and any file that its user can read, with `env()` and `file()`
  (SECRET SOURCES), and send the value to the address of a connector. This includes
  the files of the data directory: the node's keys and the sealed store. The refusal
  of a `file()` path in the data directory is #2670, before a release of ONE NODE. In
  ONE NODE, that person is the node's admin. A remote `plan` reads the sources only of
  the connectors that its subject may apply, and a local `plan` reads each connector of
  the node (SECRET SOURCES). Until #2694, a subject with `apply` and `admin` on the name
  of a new connector alone learns from a remote `plan` the error of each source that
  gives no value: whether it is set or there, whether it is empty, its size when over
  64 KiB, the OS error of a failed file call, and, for `secret()`, the absolute path of
  the data directory. Trigger for a limit that the node's operator sets, such as
  directories and variable names given at start: a spec applied by a subject that does
  not run the node.
- Each `HeaderValue` made from the token of the `influx` kind, one at each read of the
  token and one for each request, and the write buffer of hyper keep a copy of it, and
  their memory is freed without zeros
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6160767913).
  While a run lives, `secret::Value` holds the token in clear, so the copies add only
  the time until the allocator gives that memory again. Trigger for a request path
  that clears them: an attacker who reads freed heap of the node and not its live
  heap.
- Whoever can write the `ca` source of an `influx` connector can make the node trust
  a server of their own, so they can stand in for the InfluxDB server and get the
  token. A `ca` adds to the roots of the node and never replaces them, so it cannot
  make a node trust fewer servers. Over `http`, the token crosses the network in clear,
  with or without a `ca`. `plan` refuses a `ca` with an `http` address, so its user
  does not think that the token goes under TLS (INFLUX KIND). `Roots::with_pem` reads
  the text of the `ca`. Fuzzed: `connector_pem`.
- A join ticket is a secret (BQ11a).
- Rule for every crate: no secret value in a log, a status channel, an error, or
  plan output.

## Supply chain

- Each third-party crate needs a person's approval (`docs/dependencies.md`).
  `cargo deny check advisories bans licenses sources` runs in CI on each change to a
  manifest. aws-lc-rs is the only crypto provider, with one recorded exception.
- `unsafe` is denied in the workspace. Each use expects `unsafe_code` with a reason
  (`docs/claude/rust.md`, "Unsafe"). `cargo xtask miri` runs each crate that names
  `unsafe_code` under Miri, except `os` and `connector-opcua`, which Miri cannot run.
  CI starts it on a change to a `models` path of `ci.yaml`, which names each crate
  that it runs and each workspace crate that one of them depends on.
- The `fuzz/` crate has its own lock file. The `deny` job of `ci.yaml` checks it with
  `cargo deny` on each change to it.
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
in `oracles/fuzz/<target>/`. The `fuzz` job of `ci.yaml` runs each target for 60
seconds on each PR, and `fuzz.yaml` runs each target for 600 seconds each night.

| Target | Surface | Checks besides "no panic" |
| --- | --- | --- |
| `wire_header` | `wire::header::decode` | Encodes to the same bytes |
| `wire_clock` | `wire::clock::decode` | Encodes to the same bytes |
| `wire_hub_home` | `wire::hub::Home::decode`, `Open::encode`, `Credit::encode`, `keys::encode` | Each message encodes to the same bytes; each event comes in the order of a session, and each refusal is one that the order or the mode of the session gives; each valid message made from the input decodes to itself |
| `wire_blob` | `wire::blob::Server::decode`, `Requester::decode`, `Put::body`, `Reply::body`, `Body::take`, `Body::end`, `get::encode`, `Put::encode`, `Reply::encode` | Each message encodes to the same bytes; each body counts exactly the bytes of its head, and each refusal of a body is the one that its rest gives; a body ends unfinished while bytes remain; each valid message made from the input decodes to itself |
| `wire_hub_reader` | `wire::hub::Reader::decode`, `Reply::encode`, `ends::encode` | Each message encodes to the same bytes; each event comes in the order of a session; each head is live, ends at or below `u64::MAX`, and starts at or after the end of the head before it or, for the first head, the `live` of `Opened`; each refusal is one that the order or the heads of the session give; a reply of the retired kind 3 gives `Error::Kind { kind: 3 }`; the body is where `Reader::body` says; `Reader::decode` of `None` gives `Unfinished` inside a body and `Finished` at each other point, and leaves the reader as it was; each valid message made from the input decodes to itself |
| `wire_hub_frames` | `wire::hub::Frames::open`, `due`, `decode`, `received`, `draft`, `live`, with a small pool | No input panics; each frame has the path and range of its head; a layout error comes again from the next `due`; a reply of the retired kind 3 gives `Error::Kind { kind: 3 }`; a run that receives each body message in place gives the same steps as a run that gives each whole to `decode`; in a complete session, the lost seqs of each frame (`Frames::lost`) and the ranges of its frames with a sample tile the seqs from the open position on, and a latest session or a frame with no sample loses none; `Frames::live` is the open position, then the end of the last frame with a sample |
| `wire_hub_client` | `wire::hub::client::Challenge::decode`, `Signed::decode`, `Request::decode`, `Response::decode`, `First::decode`, `Read::decode`, `Open::decode`, `Reader::decode`, `Places::decode`, `place::decode`, `Body::take`, `Body::end`, and the encoders of each message | Each message encodes to the same bytes; each decoder but `Open::decode` and `Reader::decode` refuses another kind with `Error::Kind`, and `Reader::decode` refuses a first message of another kind with it; `First::decode` gives what the decoder of the kind of the message gives; each body ends at its length and nowhere else, and each refusal of a body is the one that its rest gives; a `Reader` of each mode gives nothing before the last place; a `Places` over `PLACES_MAX` gives `Error::Places`; each message of the run holds no more places than remain, and a stamp at the index place; with the last place, and at no message before it, the reader gives the start: the places of the run, their key set in place order with the index group at the index place, and the `hub::Reader` of their count and the mode; places that name a channel twice give `Error::Repeated`; a refused message leaves the reader as it was, and the end of the stream before the last place gives `Error::Finished`; each valid message made from the input decodes to itself |
| `transport_hello` | `transport::fuzzing::Hello::decode`, `Hello::encode` (feature `fuzzing`) | Gives the hello, or the refusal, that a second reader of the STREAM WIRE rules gives; its encoding decodes to itself |
| `transport_certificate` | `transport::fuzzing::peer`: the client verifier and the peer of a node's server, for a dialer's chain of 0 to 3 certificates (feature `fuzzing`) | Gives the peer that a second reader of the rules gives: a client for no certificate, none for a chain of more than one or a certificate over 1024 bytes, and else none or the node whose key follows the Ed25519 key header in the certificate; a certificate that a node issues reads back to its key, and two of it are refused. Not reached: the handshake signature, which fuzzed bytes cannot make |
| `env_host` | `env::net::host::Host::from_str` and `env::net::host::uri` | A host from either reads back from its text, and a name has no uppercase letter |
| `mesh_change` | `mesh::change::Change::decode`, and `Card::decode` and `Status::decode` through a `Join`, by `mesh::testing::round_trip_change` | Encodes to the same bytes |
| `mesh_message` | The decode of a mesh message, with its `raft` proof, chain, and entries, by `mesh::testing::round_trip_message` | Encodes to the same bytes |
| `mesh_entries` | The decode of `raft` entries one after another, as a mesh log record body and an append hold them, by `mesh::testing::round_trip_entries` | Encode to the same bytes |
| `mesh_log` | The decode of one mesh log record by `mesh::testing::round_trip_log_record`: the header, its version, and the hard state and entries of the body, after `seal_log_record` writes the length and both checks | Encodes to the same bytes |
| `mesh_log_dir` | `mesh::log::sequence` and `mesh::log::scan` on the files of a log directory, by `mesh::testing::scan_log_dir`, after `seal_log_dir` writes the checks of each record | An open of the files that an open leaves reads the same hard state and entries, and the files are a fixed point after two opens |
| `mesh_spec_name` | `mesh::driver::used::pointer` on a file name of the directory of the spec in use, by `mesh::testing::round_trip_spec_name` | A name it reads is the name of its pointer |
| `codec_series` | `codec::validate`, `codec::decode`, `codec::Decoder` | All give one result |
| `codec_encoder` | `codec::Encoder` | Its output is valid and decodes unchanged |
| `codec_string` | `codec::Encoder`, `codec::validate`, `codec::decode` on a `String` series | Each refuses at the first sample that `str::from_utf8` refuses, and at no other |
| `codec_shape` | `codec::validate`, `codec::decode`, `codec::Variable` on an array, matrix, list, `String`, or `Bytes` series | Both give one result; an array or a matrix gives the result of the series of its elements; a `String` series gives the result of a `Bytes` series or the first sample that `str::from_utf8` refuses; a valid series decodes with zeros for the padding, and encodes and decodes unchanged; `Variable` reads the samples of a valid variable series and writes them back to the same bytes |
| `codec_shape_encoder` | `codec::Encoder` on an array, matrix, list, `String`, or `Bytes` series | Gives the refusal that a second reader of the raw form gives, or a valid series that decodes unchanged with zeros for the padding; an array or a matrix encodes as the series of its elements |
| `document_encoding` | `document::encoding::decode` | Encodes to the same bytes |
| `spec_definition` | `spec::definition::Definition::decode` | Encodes to the same bytes |
| `spec_tree` | `spec::tree::get`, `apply`, `diff`, and `spec::region::definitions` on chunks from a peer | `get` agrees with a whole `diff`; `apply` gives the entries with the changes; `definitions` gives the decode of the entries, with the chunks of the tree and no other, only when `spec::region::tree` of them has the same root |
| `spec_data_type` | `spec::data_type::DataType` | Prints as the text it was read from |
| `config_hcl_read` | `config_hcl::read` | The encoding decodes to an equal document |
| `config_hcl_update` | `config_hcl::update` | Its text reads as the document; an update to its own document keeps each byte; an unread text gives the problems of `read` |
| `config_hcl_write` | `config_hcl::write` | Its text reads back as an equal document |
| `config_plan` | `config::plan::Plan::decode` | Encodes to the same bytes |
| `config_check` | `config::check` on the documents that `config_hcl::read` reads from up to three files, with the influx kind in the kind table | The same entries or the same problems for the files in either order; with no problem, one entry for each block and one for each status channel of each connector, and no other, unique in any case, each policy and connector decodes to itself, and each edge of a channel names a channel entry; each problem's span is in its file, in the order of the files, then of the source; files that pass alone, with keys that differ in more than case and no subject named as a connector in any ASCII case, pass together and give the union of their entries |
| `connector_pem` | `connector::http::Roots::with_pem` on the text of a `ca` | The same error with a UTF-8 byte order mark put at the start; an `Ok` holds a root |
| `connector_modbus_rtu` | `connector_modbus::rtu::decode_request`, `decode_reply`, `pdu::Request::decode`, `Request::decode_reply` | A request reads back unchanged; a reply has the asked count |
| `connector_modbus_tcp` | `connector_modbus::tcp::decode`, `pdu::Request::decode`, `decode_reply`, `read::groups`, `read::Group::values` | A request reads back unchanged; a reply has the asked count; reads of each register type that tile a read request group into that request, and give the values of its reply in address order, in each register order |
| `connector_opcua_decode` | `connector_opcua::fuzz::decode`: `UA_decodeBinary` of open62541, as each type of `UA_TYPES` | No memory fault or leak; a decoded value encodes to its `UA_calcSizeBinary` length, and that encoding decodes, reads exactly its length, and encodes to the same bytes. A decoded `DataValue` gives the same read of its value (`Value::of` of the kind), or the same fault, from both decodes. Until #435 is fixed, the encoding decodes with as many zeros after it as its length. |
| `ops_mcp` | `foundation mcp`, through `ops::cli` | No error, and at most one reply for each line |
| `types_name` | `Name` | Prints as the text it was read from |
| `types_selector` | `Pattern`, `Selector` | Agree with a second matcher |
| `types_stamp` | `Stamp` | Printed text reads back to the same value |
| `types_span` | `Span` | Printed text reads back to the same value |
| `types_rate` | `time::Rate` text | An `Ok` rate is reduced, has a period of 1 ns or more, and has a denominator that divides 10^9 |
| `types_range` | `Range` | Printed text reads back to the same value |
| `types_byte_size` | `byte::Size` | Printed text reads back to the same value |
| `types_channel` | `channel::Key` | Printed text reads back to the same key |
| `types_sample` | `sample::Type` | Prints as the text it was read from |
| `types_frame_ends` | `frame::Layout::from_ends`, `frame::check`, `frame::split` | Refuses exactly the ends that break a rule, with an error that names a broken rule; the layout is the one that `Layout::new` gives for the lengths; a frame drafted from the ends has them, and `split` cuts its series at them; `check` refuses exactly the ends that do not fit a body whose length the input gives, and `split` cuts a body that `check` took at them. Not reached: the panics of `split`, a body over 64 KiB |
| `buffer_open` | `Buffer::open` and `Reader::read` on an edited ring | An `Err`, or a commit survives a reopen; a read gives each path as the doc of `Reader::read` says, up to the tail, the same in one read, in steps, from inside an entry or a gap, and after a reopen. Not reached: a pool with no block, a read before a commit ends |
| `secret_sealed` | `secret::store::Sealed::put` | Takes only the one real sealed value; refuses any other bytes, name, or version; a refused `put` leaves the store as it was |
| `node_identity` | `node::sector::written` with `node::identity::fields` and `encode` (the checks `node::identity::check` and `node::admin::check`, which call `node::sector::check`), by `node::fuzz::identity` (feature `sim`), on 100 bytes, or on 96 bytes with their CRC32C; the same for `admin.key`, on 54 bytes, or on 50 bytes with their CRC32C; and the decode and encode of `address` (the check `node::address::check`), on 99 bytes, or on 95 bytes with their CRC32C | Gives an identity or an admin key exactly for bytes with the tag and the CRC32C, and an address exactly for the bytes that the encode writes, with the tag and the CRC32C, and that value encodes to the same bytes |

No target yet, because the decoder is private, not built, not reached from a file, or
not reached from the corpus: `transport::message` (#2139), the QUIC hello
(`transport::quic::hello::Hello::decode`), the file `founding` of the mesh directory
(`mesh::region::Founding::decode`, with `mesh::Member::decode` in it, #2311), the join
answer (`mesh::Member::decode` from a peer, #336 adds its target),
each connector's protocol parser, the `node_id`
text of the `opcua` kind
(`connector_opcua::node`, #2523), its `address`, which `UA_parseEndpointUrl` of
open62541 reads (`connector_opcua::Kind::parse`, #2523), the readers of the answers of
a browse and a describe, which the browse of the kind calls (`node_of`, `unit_of`,
`Answer::browsed`, and `Answer::target` in `connector_opcua::client`, #2914), the
`address` of the
`modbus` kind (`connector_modbus::Kind::parse`, #2789), the text of a browse path and
pattern, which no front end reads yet (`connector::browse`, #2827), the file `name` of
the data directory (`node::name::decode`, #2174), the file `budget` of the data
directory (`node::budget::decode`, #2174), the parsers of `/proc/self/mountinfo` and
`/proc/self/cgroup` in `os::memory` (#2226), the request of a remote program and the
reply of its node (`ops::serve`, #2563), the newest handoff record of each index in the
ring (`home::handoff::read`, #2278), the newest named reader entry of each index in
the ring (`home::named::read`, #2278), the buffer entry that a complete reader reads from
disk (`home::stored::Decoder::frame`, #2738), and the chunk processing of open62541
(`ua_securechannel.c`, #1990).
