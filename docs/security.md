# Security

The threat model of Foundation. The `red-team` session owns this file and updates it
when a surface lands. `docs/decisions.md` wins where they differ. A finding is a
GitHub issue with the `security` label and a failing test.

## What we protect

- **Commands.** A wrong or forged command moves a real machine. This is the worst
  case.
- **Telemetry in motion and in the buffer.** Its loss, change, or disclosure.
- **The spec.** It decides who may do what, and where each connector runs.
- **Secrets.** Device and store credentials, and each node's private key.
- **Availability.** A node that stops also stops the data and control paths through
  it.

## Who attacks

| Attacker | Can | Cannot |
| --- | --- | --- |
| Network peer | Reach a node's UDP and TCP port; read, drop, change, replay, and delay packets | Hold a node key or a subject key |
| Unknown client | Open a TLS session with no certificate (`Peer::Client`) | Sign a hello for a subject in the spec |
| Subject | Sign hellos and session opens with a key in the spec | Go past its `access` allows |
| Member node | Hold a node key in the spec; forward for others; vote, if a voter | Hold another node's key |
| Device | Send any bytes to a connector | Reach the core except through `hub` |
| Local user | Read and write the node's files | Read memory of the process |
| Dependency | Ship hostile or defective code in a crate we build | |

Out of scope in v1: a voter that lies in consensus (Raft is not Byzantine), a hostile
operating system or hardware, and end-to-end integrity of frames across forwarding
nodes (BQ12).

## Trust boundaries and their state

Each line names the check that holds the boundary, the crate that owns it, and its
state on `main`.

### Network to `transport`

- TLS 1.3 only, ALPN `foundation/1`, no resumption, no 0-RTT (NODE KEY TLS). A peer is
  the Ed25519 key in its leaf certificate. Names, dates, and the issuer are not
  checked. Landed in `tls`; the carriers are not built (#55, #68).
- `transport` accepts every key and every client with no certificate. Admission is
  the caller's job. Until `node` admits a peer, the peer must not make the node hold
  memory or do work out of proportion to the bytes it sent.
- A message on a stream is a length and then bytes. The length is the peer's choice,
  up to `message_bytes_max`.
- Open: #227 (a key of small order needs no private key), #228 (length prefixes hold
  and fragment the shard's pool).
- Not decided: whether TLS ends at a relay or at the far node. If it ends at the
  relay, the relay can claim any `Peer::Node`. No limit on handshakes before
  admission is decided either; each costs one ML-KEM operation and one signature
  check.

### `transport` to protocols

- The first message of a stream, and each datagram, starts with a `wire` header
  (PROTOCOL HEADER). `node` stops a stream whose header is not valid. A client opens
  only hub streams. `wire::header` landed; the dispatch table in `node` is not built.

### Subject to owner

- A remote subject signs a short-lived hello and each session open. The gateway
  forwards the signatures. The owner checks them against the subject's keys in the
  spec (BQ12). A node acts only as itself or as a connector placed on it. Not built
  (`access`, `hub`).
- To attack when it lands: a replayed hello, a hello for another gateway, a session
  open with no fresh signature, a forwarding node that swaps the subject, and any
  read or write path that does not reach `access`.

### Node to node

- Node-to-node traffic is authorized by role (BQ12). `raft` does not check that the
  sender of a message is a voter, because a voter change can be in flight. So the
  mesh protocol must prove that a sender is a member of the group before its message
  reaches `Raft::step`. Not built (`mesh`).
- `raft` trusts each field of a message. Open: #232.

### Files to the spec

- HCL text becomes a `Document` (`config-hcl`), and a `Document` has one canonical
  encoding (`document`). Both readers bound nesting at 64 levels. Fuzzed:
  `config_hcl_read`, `document_encoding`.
- Config names secrets and never holds their values (K4).

### Disk to `buffer`

- The disk can tear, cut, flip, or zero bytes, and can hold records from an older lap
  of the ring. A chained CRC32C finds these. It does not stop a local user who writes
  the file: the CRC is not a secret, and a header block has no tie to its ring.
- Open: #234. The engine is not built (#161).

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
- A secret value is sealed to each node that may use it, and only `ctx.secret(name)`
  reads it (BQ16, SECRET STORES AS ADAPTERS). Not built (`secret`).
- Rule for every crate: no secret value in a log, a status channel, an error, or
  plan output.

## Supply chain

- Each third-party crate needs a person's approval (`docs/dependencies.md`).
  `cargo deny check advisories bans licenses sources` runs in CI on each change to a
  manifest. aws-lc-rs is the only crypto provider, with one recorded exception.
- `unsafe` is denied in the workspace. The crates that allow it (`block`, `ring`,
  `counting`) run under Miri in CI.
- The `fuzz/` crate has its own lock file, which `cargo deny` does not read (#252).

## Fuzz targets

One target for each decoder of outside input (`docs/claude/testing.md`). Inputs are
in `oracles/fuzz/<target>/`. The crate is in #241; its CI job is #252.

| Target | Reads | Checks besides "no panic" |
| --- | --- | --- |
| `wire_header` | `wire::header::decode` | Encodes to the same bytes |
| `codec_series` | `codec::validate`, `codec::decode` | Both give one result |
| `codec_encoder` | `codec::Encoder` | Its output is valid and decodes unchanged |
| `document_encoding` | `document::encoding::decode` | Encodes to the same bytes |
| `config_hcl_read` | `config_hcl::read` | The encoding decodes to an equal document |
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
