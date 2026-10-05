# Security

The threat model of Foundation. The `red-team` session owns this file and updates it
when a surface lands. `docs/decisions.md` wins where they differ. A defect that an
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
| Voter | Vote and stall its region | Forge a spec change or move a home outside placement |
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
- `transport` accepts every Ed25519 key, and every client with no certificate.
  Admission is the caller's job. Until `node` admits a peer, the peer must not make
  the node hold memory or do work out of proportion to the bytes it sent.
- A message on a stream is a length and then bytes. The length is the peer's choice,
  up to `message_bytes_max`.
- A key of small order needs no private key. `types::node::PublicKey::new` refuses
  each one, so every check site gets the check from the type.
- Each shard signs stateless resets with one key for the node. So the router must
  hand a datagram only to the shard that owns its connection ID, by the first byte
  (#77). If not, a shard signs a valid reset for a live connection of another shard.
- Open: #228 (length prefixes hold and fragment the shard's pool), #298 (junk from
  one address stops every stateless reset; a small datagram of an unknown version
  gets a reply), #299 (a peer makes the node hold junk certificates for a session).
- Not decided: a limit on handshakes before admission. Each one costs the node a key
  exchange and one signature, and one signature check more when the peer sends a
  certificate.

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
  not reach `access`.

### Node to node

- Node-to-node traffic is authorized by role (BQ12). A new node joins only with a
  signed ticket, and voters record membership (BQ11a). Not built (`mesh`).
- `apply` signs the plan hash, and every node checks every change record (BQ12). So
  a voter that lies can stall its region, and cannot change access, keys, or
  placement. Not built (`spec`).
- `raft` checks that the sender of a reply is a voter. It does not check the sender
  of a request (`PreVote`, `Vote`, `Heartbeat`, `Append`), and it trusts each field of
  a message. Open: #232, which also asks who proves that a sender is in the group.
- `estimate` combines the bounds of time sources. Open: #344 (one lying source of
  three puts a small bound inside the honest overlap, and the estimate follows it).
- The `clock`, `replica`, and `blob` protocols are not built. To attack when they
  land: a time source that reports a small bound to steer the clocks that follow it
  (R6), and a binary that a peer serves under a hash it does not match (C9d).

### Files to the spec

- HCL text becomes a `Document` (`config-hcl`), and a `Document` has one canonical
  encoding (`document`). Both readers bound nesting at 64 levels.
  `config_hcl::write` gives text that reads back as an equal `Document`. Fuzzed:
  `config_hcl_read`, `config_hcl_write`, `document_encoding`.
- A person reviews a spec file before `apply`. Text that shows one thing and reads
  as another defeats that review. Open questions: #301 (a lone `\r` in a comment,
  bidirectional controls, keys compared by bytes).
- Config names secrets and never holds their values (K4).

### Disk to `buffer`

- The disk can tear, cut, flip, or zero bytes, and can hold records from an older lap
  of the ring. A chained CRC32C finds these. It does not stop a local user who writes
  the file: the CRC is not a secret, and a header block has no tie to its ring.
- The engine is not built (#161). #234 and #300 are robustness defects of this
  boundary: they need a writer of the file, so they do not have the `security`
  label.

### Device to connector

- Each protocol parser reads bytes from a device. A connector reaches the core only
  through `hub`. Not built. Each parser gets a fuzz target when it lands.

### Encoded series

- `codec::validate` and `codec::decode` read series from peers and from disk. A
  series cannot make `decode` write outside `out`. Fuzzed: `codec_series`,
  `codec_encoder`.

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
  may cache values sealed to it, which delays revocation. Not built (`secret`).
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
(`docs/claude/testing.md`). Inputs are in `oracles/fuzz/<target>/`. The CI job is #252.

| Target | Reads | Checks besides "no panic" |
| --- | --- | --- |
| `wire_header` | `wire::header::decode` | Encodes to the same bytes |
| `codec_series` | `codec::validate`, `codec::decode` | Both give one result |
| `codec_encoder` | `codec::Encoder` | Its output is valid and decodes unchanged |
| `document_encoding` | `document::encoding::decode` | Encodes to the same bytes |
| `config_hcl_read` | `config_hcl::read` | The encoding decodes to an equal document |
| `config_hcl_write` | `config_hcl::write` | Its text reads back as an equal document |
| `types_name` | `Name` | Prints as the text it was read from |
| `types_selector` | `Pattern`, `Selector` | Agree with a second matcher |
| `types_stamp` | `Stamp` | Printed text reads back to the same value |
| `types_span` | `Span` | Printed text reads back to the same value |
| `types_range` | `Range` | Printed text reads back to the same value |
| `types_channel` | `channel::Key` | Printed text reads back to the same key |

No target yet, because the decoder is private or not built: `transport::message`
and `tls` (#55), the `buffer` records (#161), `raft` messages (their encoding is in
`mesh`), `spec` tree chunks (#64), `types::time::Rate`, and each connector's
protocol parser.
