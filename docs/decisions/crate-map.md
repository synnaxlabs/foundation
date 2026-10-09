# Crate map

Rules:

1. A crate depends only on crates earlier in its own layer or on lower layers.
2. Layer 1 is pure: no I/O, no clock reads, no threads, no globals. It decides.
3. Layer 2 drives the mesh's own disk, peer network, and clock. It does.
4. Layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
5. Layer 4 may depend on anything below it.
6. Upward flow goes only through values the upper crate pulls (watches, streams).
   Seams that lower crates define and upper crates implement (`env` traits) are
   injected downward.
7. Only `os` touches the real clock, files, network, serial ports, randomness, and
   threads, through the `env` seams it implements. Only `clock` reads wall time
   through `env`; everyone else asks `clock`. Only `node` builds real seams, and only
   `sim` builds simulated ones. Below `hub`, only `home` writes channels, and only its
   companion samples.
8. Tests follow the same rules, with these extra dev-dependencies only: any crate may
   take `sim` and `counting`, `connector-ni` may take `daqmx-stub`, `hub` may take
   `buffer`, so its tests build a real `home::Shard`, and `access` may take `document`,
   so its tests build a `spec::connector::Connector` (`laptop.architect`,
   2026-10-08T03:01:36Z:
   https://github.com/synnaxlabs/foundation/issues/810#issuecomment-6051285927), and
   `config` may take `config-hcl` and `connector-influx`, so its tests check a real file
   with a real kind (`laptop.architect-2`, 2026-10-08T03:02Z:
   https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152), and
   `ops` may take `config-hcl`, so its plan tests read a real file
   (`laptop.architect-2`, 2026-10-08T16:00:18Z:
   https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063892745), and
   `ops` may take `transport`, so its tests open a real `Mesh` (`laptop.architect-2`,
   2026-10-08T23:21:45Z:
   https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6070995340), and
   `hub` may take `blob`, so its region tests open a real `mesh::Mesh` on a
   `blob::Store` (`laptop.architect`, 2026-10-08T19:18:09Z:
   https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6067290747). A crate
   may also take itself, so its tests and benches build with its own `sim` feature
   (STORED BENCH; `laptop.architect`, 2026-10-08T01:01:28Z:
   https://github.com/synnaxlabs/foundation/pull/1568#issuecomment-6049989224). The
   `hub` edge was decided by the architect (#340). Lost: `buffer` in the `hub` row (hub
   code could call the ring), the hub tests in `node`, and a second way to build a shard
   in `home`.

Order: layer 1 (`block`, `ring`, `counting`) -> `types` -> (`env`, `document`, `raft`,
`estimate`, `control`, `delivery`) -> `codec` -> `wire` -> `spec` -> `access`; layer 2
`os` -> (`transport`, `buffer`) -> (`clock`, `blob`, `sim`) -> `mesh` -> (`home`,
`replica`) -> `hub`; layer 3 `secret` -> `connector` -> `connector-<kind>`; layer 4
(`config-hcl`, `config`) -> `ops` -> `node`.

| Layer | Crate | Job (one sentence) | Allowed dependencies |
| --- | --- | --- | --- |
| 1 | `block` | Owns pools of preallocated, aligned buffers (`Pool`, `Unique`, `Block`, one refcount per frame, offsets only) and their unsafe memory code. | none |
| 1 | `ring` | Carries handles between shards through bounded single-producer, single-consumer rings, owns the wake protocol (loom-checked) and the `latest` cell that one shard writes and every shard reads, and holds its own unsafe slot code (memory delegation, 2026-10-04). A consumer parks at once: the shard idle loop owns the spin window through `try_pop` (#46). | none |
| 1 | `counting` | Counts heap allocations so tests and benchmarks can assert that code does not allocate, counts the heap bytes held so tests can bound the memory of a structure, finds freed blocks that hold given bytes so tests can assert that code erases a secret, and holds the `unsafe impl GlobalAlloc` of every crate after `block`, which keeps its own. A dev-dependency only. | none |
| 1 | `types` | Defines byte-level values: time, byte sizes, sample types, series, frames, key sets, masks, views, keys, slots, quality, names, node keys, Ed25519 public and private keys, with the derive, the sign, and the verify, subject hellos, connection keys, control authority, content digests, the one selector matcher, and the one quote form for text in diagnostics. | `block` |
| 1 | `env` | Defines the injected seams for monotonic time, the OS wall clock (read only by `clock`), files, the network, serial ports, randomness, shards, dedicated threads, and task spawning. | `types`, `block` |
| 1 | `document` | Defines the syntax-neutral Document with source positions, diagnostics, shared value readers, and its canonical encoding. | `types` |
| 1 | `raft` | Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing about specs. | `types` |
| 1 | `estimate` | Computes clock offset and error bounds from measurements, the peer exchange, and device oscillator fits, slews mesh time, and chooses what mesh time follows. | `types` |
| 1 | `control` | Decides who holds control of an index: authority, ties, control leases, handoffs, start state after failover. | `types` |
| 1 | `delivery` | Keeps each reader's state per index: positions, credits, live frames for complete readers, latest mailbox, holds, floors, position records, masks. | `types`, `block` |
| 1 | `codec` | Compresses and checks one series: per-vector selection, codecs, header validation, format version. | `types`, `block` |
| 1 | `wire` | Defines every message between two nodes, or between a program and the node it connects to, except the bodies of the mesh protocol, which `mesh` encodes (MESH WIRE): per-connection short numbers, predicted seq and counts, session, credit, and replication messages, format version. | `types`, `block`, `codec` |
| 1 | `spec` | Defines the definitions (channels, types, units, connectors with opaque config, regions, policies, open folders), the prolly tree, hashes, diffs, and `spec::resolve`. | `types`, `document` |
| 1 | `access` | Decides whether a proof is of its subject (signed hellos and requests), and whether a subject may do an action on a name: union of allows, authority cap. | `types`, `spec`; `document` as a dev-dependency only |
| 2 | `os` | Implements the `env` seams and `block::Memory` on the real operating system: monotonic and wall clocks, files, sockets, serial ports, memory, randomness, and threads. The only crate allowed to call them. Holds its own unsafe memory code in `os::memory` (BLOCK MEMORY), and the OS calls of its clock and wall clock in `os::clock` and `os::wall` (#117). On macOS, `os::allocate` holds one `fcntl(F_PREALLOCATE)` call, because `rustix` can allocate only part of a new file (architect, #931, https://github.com/synnaxlabs/foundation/issues/931#issuecomment-6030986099). `os::net` holds one `setsockopt(TCP_NOTSENT_LOWAT)` call, because `rustix` does not give that option (laptop.architect-2, 2026-10-08 02:32 UTC, #120, https://github.com/synnaxlabs/foundation/issues/120#issuecomment-6050971843). `os::net::resolve` holds the `getaddrinfo` and `freeaddrinfo` calls, because std gives one error for `EAI_NONAME` and `EAI_AGAIN` (laptop.architect-2, 2026-10-08 16:52 UTC, #1095, https://github.com/synnaxlabs/foundation/issues/1095#issuecomment-6064802287). | `env`, `types`, `block` |
| 2 | `transport` | Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS over TCP, relays, and diodes on the `env::net` seam; never calls up. | `env`, `types`, `block` |
| 2 | `buffer` | Stores each index's log durably within the disk budget (write-ahead ring, segments, trimming, floors, `append`) through a per-OS driver. | `env`, `types`, `block`, `codec` |
| 2 | `clock` | Runs time source adapters and the peer exchange, feeds `estimate`, and serves mesh time as an interval. | `ring`, `env`, `types`, `estimate`, `wire`, `transport` |
| 2 | `blob` | Stores content by hash and fetches it from peers (spec chunks, binaries). | `env`, `types`, `block`, `wire`, `transport` |
| 2 | `sim` | Simulates the `env` seams (time, randomness, scheduling, files, network, serial lines) with a deterministic scheduler and fault injection; ships behind a feature. | `env`, `types`, `block` |
| 2 | `mesh` | Agrees per region, through `raft`, on spec pointers, delegations, and runtime state (membership, node leases, homes, seq blocks, index history, secret ciphertexts, tickets, versions, rollout lock, format flag); serves snapshots, watches, effective settings, and the changes channels. | `env`, `types`, `block`, `raft`, `spec`, `access`, `wire`, `transport`, `clock`, `blob` |
| 2 | `home` | Runs the per-index write path (time checks, seq, fence, control, storage, fan-out), crash-recovery and copy-mode opens, and companion writes. | `env`, `types`, `block`, `ring`, `control`, `delivery`, `codec`, `spec`, `access`, `buffer`, `clock`, `mesh` |
| 2 | `replica` | Receives an index's log from its home on a standby or copy node and stores it with `append`. | `env`, `types`, `block`, `wire`, `transport`, `buffer`, `mesh` |
| 2 | `hub` | Is the one path for every read and write: sessions across homes, routing, live selectors, the server loop, authentication, encode and decode once, raw cursors for replicas, re-index stitching, the layer-3 window, and the client session of a program. | `access`, `env`, `types`, `block`, `ring`, `codec`, `wire`, `spec`, `transport`, `clock`, `mesh`, `home`; `buffer` and `blob` as dev-dependencies only |
| 3 | `secret` | Resolves a named secret on the node that runs a connector, through store adapters chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`. Seals a value to a node's seal key, and opens it. | layer 1 |
| 3 | `connector` | Defines the kind contract (parse, check, discover, run), the thin supervisor, `ctx`, the component library, and the compositions. | layer 1, `hub`, `secret` |
| 3 | `connector-<kind>` | Translates one protocol, device family, store, or the calculation engine into channels. | layer 1, `hub`, `connector`; vendor libraries behind build flags, except a library loaded at run time, which links nothing |
| 3 | `daqmx-stub` | Stands in for NI's `libnidaqmx.so` in the tests of `connector-ni`, built as a shared library and as a Rust library. A dev-dependency of `connector-ni` only. | none |
| 4 | `config-hcl` | Reads and writes HCL files as Documents. | `types`, `document` |
| 4 | `config` | Checks core definitions in Documents, expands templates, hands connector blocks to kinds, and computes plans, explains, and exports. | layer 1, `connector`; `config-hcl` and `connector-influx` as dev-dependencies only |
| 4 | `ops` | Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and runs each operation on the node that must run it. | `config`, `connector`, `hub`, `mesh`, `blob`, `sim`, layer 1 |
| 4 | `acceptance` | Runs the MVP acceptance scenarios against whole meshes built from `node`. Test-only. | all crates |
| 4 | `node` | Is the composition root: real seams, pools and shards, all tables (kinds, front ends, time sources, secret stores), the status collector, process lifecycle, and upgrades. | all crates |

`fuzz` holds the fuzz targets, outside the workspace. It is test-only, builds on the
pinned nightly, may depend on any crate, and no crate depends on it (#252;
`laptop.architect`, 2026-10-09T04:18Z,
https://github.com/synnaxlabs/foundation/pull/2099#issuecomment-6074164278).

Outside the binary: the Rust SDK reuses `block`, `types`, `codec`, and `wire`; other
SDKs hand-write their data path against golden vectors (D12).
