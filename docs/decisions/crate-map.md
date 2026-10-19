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
   through `env`; everyone else asks `clock`. Only `node` builds real seams in the
   binary, and `sdk` builds those of a client from `os` (SDK CLIENT). Only `sim`
   builds simulated ones. Below `hub`, only `home` writes channels, and only its
   companion samples.
8. Tests follow the same rules, with these extra dev-dependencies only: any crate may
   take `sim`, `counting`, and `waker` (`laptop.architect`, 2026-10-10 06:25 UTC,
   #2254, https://github.com/synnaxlabs/foundation/issues/2254#issuecomment-6094631943),
   `connector-ni` may take `daqmx-stub`, `hub` may take `buffer`, so its tests build a
   real `home::Shard`, and `access` may take `document`, so its tests build a
   `spec::connector::Connector` (`laptop.architect`, 2026-10-08T03:01:36Z:
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
   `ops` may take `clock`, so its tests give a real `Mesh` its mesh time
   (`laptop.architect-2`, 2026-10-19T03:26:01Z:
   https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6258983937), and
   `hub` may take `blob`, so its region tests open a real `mesh::Mesh` on a
   `blob::Store` (`laptop.architect`, 2026-10-08T19:18:09Z:
   https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6067290747), and
   `program` may take `hub` with `sim`, so its tests run a session against the real
   serve of a hub (`laptop.architect`, 2026-10-15T18:23:47Z,
   https://github.com/synnaxlabs/foundation/pull/2724#issuecomment-6199257488), and
   `spec`, so its tests build the rules that admit a hello (`laptop.architect`,
   2026-10-15T19:16:41Z,
   https://github.com/synnaxlabs/foundation/pull/2724#issuecomment-6200305120), and
   `codec`, so its read tests decode the encoded series of each frame
   (`laptop.architect`, 2026-10-15T23:36:14Z,
   https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6204521616).
   `node` may take `sdk`, so its tests run the client of a program against a real node
   (`laptop.architect`, 2026-10-16T21:33:33Z,
   https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225122027).
   `sdk-python` may take `transport`, so its test of `error` builds a bind error
   (`laptop.architect-2`, 2026-10-17T21:48:03Z,
   https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241731905).
   A crate may also take itself, so its tests and benches build with its own `sim`
   feature (STORED BENCH; `laptop.architect`, 2026-10-08T01:01:28Z:
   https://github.com/synnaxlabs/foundation/pull/1568#issuecomment-6049989224). A
   layer 3 crate may take `hub` with `sim`, so its tests build a hub through
   `hub::testing::open`, on the shard of `home::testing::shard` (`laptop.architect`,
   2026-10-08T17:50:53Z:
   https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6065807610). It
   supersedes the Lost item "a second way to build a shard in `home`" of the `hub` edge,
   which the architect decided (#340:
   https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6030590185). Lost:
   `buffer` in the `hub` row (hub code could call the ring), the hub tests in `node`,
   and a builder that takes a `sim::node::Node`, which adds an edge on `sim`.
9. A crate of layer 2, 3, or 4 may take `tracing` and write events. A layer 1 crate
   writes no event: it returns a value that its caller logs. `env` (layer 1) takes
   `tracing` only for the `Dispatch` type that its `Shards` and `Threads` hold. Only
   `node` takes `tracing-subscriber` and builds the dispatch of a node, and no crate
   sets a default for the whole process (`laptop.architect`, 2026-10-13T21:14:39Z,
   #2606: https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155332793;
   rule meaning (`laptop.director`, 2026-10-13T21:12:40Z):
   https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155299486).
10. A crate's `sim` feature is test-only: only a dev-dependency, the fuzz crate, or
    another `sim` feature turns it on. The BQ19 feature is `simulate`; it turns on
    `dep:sim` and other `simulate` features, never a `sim` feature. `cargo xtask`
    checks this (#1570) (`laptop.architect`, 2026-10-08T02:11:30Z:
    https://github.com/synnaxlabs/foundation/issues/1570#issuecomment-6050751380).
    The check resolves with all features on, walks from each member along each edge
    that is not only a dev-dependency, and checks each local crate that it reaches: a
    member, a benchmark, `xtask`, or a path crate outside the workspace. A build
    dependency counts, as it runs in each product build, and so does a benchmark or
    `xtask`, as `cargo build --workspace` unifies its features with the binary's
    (`laptop.architect`, 2026-10-09T20:33:17Z:
    https://github.com/synnaxlabs/foundation/pull/2140#issuecomment-6088787290).

Order: layer 1 (`block`, `ring`, `counting`, `waker`) -> `types` -> (`env`, `document`,
`raft`, `estimate`, `control`, `delivery`) -> `codec` -> `wire` -> `spec` -> `access`;
layer 2 (`os`, `sector`) -> (`transport`, `buffer`) -> (`clock`, `blob`, `sim`,
`program`) -> `mesh` -> (`home`, `replica`) -> `hub`; layer 3 `secret` -> `connector`
-> `connector-<kind>`; layer 4 (`config-hcl`, `config`) -> `ops` -> `node` -> `sdk` ->
`sdk-python`.

| Layer | Crate | Job (one sentence) | Allowed dependencies |
| --- | --- | --- | --- |
| 1 | `block` | Owns pools of preallocated, aligned buffers (`Pool`, `Unique`, `Block`, one refcount per frame, offsets only) and their unsafe memory code. | none |
| 1 | `ring` | Carries handles between shards through bounded single-producer, single-consumer rings, owns the wake protocol (loom-checked) and the `latest` cell that one shard writes and every shard reads, and holds its own unsafe slot code (memory delegation, 2026-10-04). A consumer parks at once: the shard idle loop owns the spin window through `try_pop` (#46). | none |
| 1 | `counting` | Counts heap allocations so tests and benchmarks can assert that code does not allocate, counts the heap bytes held and given out so tests can bound the memory of a structure and what code allocates, finds freed blocks that hold given bytes so tests can assert that code erases a secret, counts the allocations of one thread so a test binary with a harness can assert that its caller's thread does not allocate (`Local`, Unix only, which names the thread with `libc::pthread_self`, approved by `laptop.architect` in https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6256055880 item 1), and holds the `unsafe impl GlobalAlloc` of every crate, except the allocation test of `block` (`crates/block/tests/alloc`), which keeps its own. A dev-dependency only. | none |
| 1 | `waker` | Makes wakers for tests that own a value that is not `Send`, so a test can give a future a waker whose drop runs its code. Holds the `unsafe` of those wakers. A waker used on another thread aborts the process. A dev-dependency only (`laptop.architect`, 2026-10-10 06:25 UTC, #2254, https://github.com/synnaxlabs/foundation/issues/2254#issuecomment-6094631943, 06:28 UTC, https://github.com/synnaxlabs/foundation/issues/2254#issuecomment-6094654313; the abort, 2026-10-10 09:11 UTC, #2265, https://github.com/synnaxlabs/foundation/pull/2265#issuecomment-6095996730, items 3 and 4 withdrawn 09:19 UTC, https://github.com/synnaxlabs/foundation/pull/2265#issuecomment-6096058772). | none |
| 1 | `types` | Defines byte-level values: time, byte sizes, sample types, series, frames, key sets, masks, views, keys, slots, quality, names, node keys, Ed25519 public and private keys, with the derive, the sign, and the verify, subject hellos, connection keys, control authority, content digests, the mark where a read of a log stands (`log::Mark`), the one selector matcher, and the one quote form for text in diagnostics. It also holds the wakers of the futures that wait for one event (`wait::Set`) (`laptop.architect`, 2026-10-10 05:33 UTC, #1701, https://github.com/synnaxlabs/foundation/issues/1701#issuecomment-6094250342). | `block` |
| 1 | `env` | Defines the injected seams for monotonic time, the OS wall clock (read only by `clock`), files, the network, serial ports, randomness, shards, dedicated threads, and task spawning. Its tests hold one `unsafe` module, `tasks::tests::hooked`: a waker whose clone and drop run a closure of the test, and the tests that use it. Each test of `env` runs under Miri (laptop.architect-2, 2026-10-10 03:57 UTC, #2218, https://github.com/synnaxlabs/foundation/pull/2218#issuecomment-6093523974). | `types`, `block` |
| 1 | `document` | Defines the syntax-neutral Document with source positions, diagnostics, shared value readers, and its canonical encoding. | `types` |
| 1 | `raft` | Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing about specs. | `types` |
| 1 | `estimate` | Computes clock offset and error bounds from measurements, the peer exchange, and device oscillator fits, slews mesh time, and chooses what mesh time follows. | `types` |
| 1 | `control` | Decides who holds control of an index: authority, ties, control leases, handoffs, start state after failover. | `types` |
| 1 | `delivery` | Keeps each reader's state per index: positions, credits, live frames for complete readers, latest mailbox, holds, position records, masks. | `types`, `block` |
| 1 | `codec` | Compresses and checks one series: per-vector selection, codecs, header validation, format version. | `types`, `block` |
| 1 | `wire` | Defines every message between two nodes, or between a program and the node it connects to, except the bodies of the mesh protocol, which `mesh` encodes (MESH WIRE): per-connection short numbers, predicted seq and counts, session, credit, and replication messages, format version, and builds the frames of a reader session (HUB WIRE) and of a program's write at the node (HUB CLIENT) from their messages. | `types`, `block`, `codec` |
| 1 | `spec` | Defines the definitions (channels, types, units, connectors with opaque config, regions, policies, open folders), the prolly tree, hashes, diffs, and `spec::resolve`. | `types`, `document` |
| 1 | `access` | Decides whether a proof is of its subject (signed hellos and requests), and whether a subject may do an action on a name: union of allows, authority cap. | `types`, `spec`; `document` as a dev-dependency only |
| 2 | `os` | Implements the `env` seams and `block::Memory` on the real operating system: monotonic and wall clocks, files, sockets, serial ports, memory, randomness, and threads. The only crate allowed to call them. Holds its own unsafe memory code in `os::memory` (BLOCK MEMORY), and the OS calls of its clock and wall clock in `os::clock` and `os::wall` (#117). On macOS, `os::allocate` holds one `fcntl(F_PREALLOCATE)` call, because `rustix` can allocate only part of a new file (architect, #931, https://github.com/synnaxlabs/foundation/issues/931#issuecomment-6030986099). `os::net` holds one `setsockopt(TCP_NOTSENT_LOWAT)` call, because `rustix` does not give that option (laptop.architect-2, 2026-10-08 02:32 UTC, #120, https://github.com/synnaxlabs/foundation/issues/120#issuecomment-6050971843). `os::net::resolve` holds the `getaddrinfo` and `freeaddrinfo` calls, because std gives one error for `EAI_NONAME` and `EAI_AGAIN` (laptop.architect-2, 2026-10-08 16:52 UTC, #1095, https://github.com/synnaxlabs/foundation/issues/1095#issuecomment-6064802287). `os::signal` holds the signal mask and `sigwait` calls that hold SIGINT and SIGTERM, because `rustix` gives neither (laptop.architect-2, #1732, https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545). On macOS, `os::memory::available` holds the `mach_host_self` and `host_statistics64` calls (laptop.architect-2, 2026-10-09T22:29:12Z, #2191, https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6090312136), because `rustix` has no Mach calls. `mach_host_self` comes from `mach2`, and `host_statistics64` from `libc`, because `mach2` 0.7.0 has no `host_statistics64` and `libc` does not deprecate it (laptop.architect-2, 2026-10-09T23:26:00Z, https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6090979726). On macOS, `os::memory` holds one `madvise(MADV_FREE)` call, because `rustix` gives only the POSIX advice there (laptop.architect-2, 2026-10-16T19:41:36Z, #2907, https://github.com/synnaxlabs/foundation/pull/2907#issuecomment-6223324987). `os::roots` reads the CA certificates of the OS through `rustls-native-certs` (laptop.architect-2, 2026-10-16T10:09:09Z, #2702, https://github.com/synnaxlabs/foundation/issues/2702#issuecomment-6213160739). Under the feature `sim`, on Linux, `os::memory::available_under` reads the files of `/proc` and of the cgroups under a given root, so the tests of that code go through a public call (laptop.architect-2, 2026-10-10T04:00:18Z, #2191, https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6093544655). | `env`, `types`, `block` |
| 2 | `sector` | Reads and makes the small checked files of a data directory, each of one sector with a tag and a CRC32C, and holds the formats of `address` and `admin.key` and the one read of both that a client on the node's host calls (laptop.architect, 2026-10-18T16:48:30Z, #3212, https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252501317; the name, https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252515306). | `env`, `types`, `block` |
| 2 | `transport` | Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS over TCP, relays, and diodes on the `env::net` seam; never calls up. | `env`, `types`, `block` |
| 2 | `buffer` | Stores each index's log durably within the disk budget (write-ahead ring, segments, trimming, floors, `append`) through a per-OS driver. | `env`, `types`, `block`, `codec` |
| 2 | `clock` | Runs time source adapters and the peer exchange, feeds `estimate`, serves mesh time as an interval, and gives the OS wall time and mesh time of each log line. | `ring`, `env`, `types`, `estimate`, `wire`, `transport` |
| 2 | `blob` | Stores content by hash and fetches it from peers (spec chunks, binaries). | `env`, `types`, `block`, `wire`, `transport` |
| 2 | `sim` | Simulates the `env` seams (time, randomness, scheduling, files, network, serial lines) with a deterministic scheduler and fault injection; ships behind a feature. | `env`, `types`, `block` |
| 2 | `program` | Runs the session of a program with one node: dials it, keeps the subject's hello admitted, and sends signed requests, reads, and writes. | `env`, `types`, `block`, `wire`, `transport`, `access`; `hub`, `sim`, `spec`, and `codec` as dev-dependencies only |
| 2 | `mesh` | Agrees per region, through `raft`, on spec pointers, delegations, and runtime state (membership, node leases, homes, seq blocks, index history, secret ciphertexts, tickets, versions, rollout lock, format flag); serves snapshots, watches, effective settings, and the changes channels. | `env`, `types`, `block`, `raft`, `spec`, `access`, `wire`, `transport`, `clock`, `blob` |
| 2 | `home` | Runs the per-index write path (time checks, seq, fence, control, storage, fan-out), crash-recovery and copy-mode opens, and companion writes. | `env`, `types`, `block`, `ring`, `control`, `delivery`, `codec`, `spec`, `access`, `buffer`, `clock`, `mesh` |
| 2 | `replica` | Receives an index's log from its home on a standby or copy node and stores it with `append`. | `env`, `types`, `block`, `wire`, `transport`, `buffer`, `mesh` |
| 2 | `hub` | Is the one path for every read and write: sessions across homes, routing, live selectors, the server loop, authentication, encode and decode once, raw cursors for replicas, re-index stitching, and the layer-3 window. | `access`, `env`, `types`, `block`, `ring`, `codec`, `wire`, `spec`, `transport`, `clock`, `mesh`, `home`; `buffer` and `blob` as dev-dependencies only |
| 3 | `secret` | Resolves the source of a secret on the node that runs a connector: a variable of the node's environment, a file, or a name in a store adapter chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`. Seals a value to a node's seal key, and opens it. | layer 1 |
| 3 | `connector` | Defines the kind contract (parse, check, discover, run, browse, block, tag), the browse walk, the channel names of an import, the thin supervisor, `ctx`, the component library, and the compositions. | layer 1, `hub`, `secret` |
| 3 | `connector-<kind>` | Translates one protocol, device family, store, or the calculation engine into channels. | layer 1, `hub`, `connector`; vendor libraries behind build flags, except a library loaded at run time, which links nothing |
| 3 | `daqmx-stub` | Stands in for NI's `libnidaqmx.so` in the tests of `connector-ni`, built as a shared library and as a Rust library. A dev-dependency of `connector-ni` only. | none |
| 4 | `config-hcl` | Reads and writes HCL files as Documents. | `types`, `document` |
| 4 | `config` | Checks core definitions in Documents, expands templates, hands connector blocks to kinds, and computes plans, explains, and exports. | layer 1, `connector`; `config-hcl` and `connector-influx` as dev-dependencies only |
| 4 | `ops` | Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and runs each operation on the node that must run it. | `config`, `connector`, `secret`, `hub`, `mesh`, `blob`, `sim`, layer 1 |
| 4 | `acceptance` | Runs the MVP acceptance scenarios against whole meshes built from `node`. Test-only. | all crates |
| 4 | `node` | Is the composition root: real seams, pools and shards, all tables (kinds, front ends, time sources, secret stores), the status collector, process lifecycle, and upgrades. | all crates |
| 4 | `sdk` | Is the Rust client of a program, outside the binary: it runs a `program::Session` on a shard of `os`, reads the node and its first admin from a data directory with `sector`, and the Python SDK wraps it. | `program`, `transport`, `os`, `sector`, `spec`, `env`, `types`, `block` (`sector`: https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252501317; `spec`: laptop.architect-2, item 2 of https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252530569) |
| 4 | `sdk-python` | Is the Python SDK, outside the binary: a PyO3 module that wraps `sdk::Client` for Python programs. Holds one `unsafe` read in `write::raw`, of each sample of a `numpy` array that is not aligned or not contiguous (laptop.architect-2, 2026-10-19T13:03:37Z, https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267870899). | `sdk`, `program`, `sector`, `types` (`program`: laptop.architect-2, item 2 of https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257620825, and laptop.architect, https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257630032; `sector`: laptop.architect-2, https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252530569, and laptop.architect, https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6252972067); `transport` as a dev-dependency only (https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241731905) |

`fuzz` holds the fuzz targets, outside the workspace. It is test-only, builds on the
pinned nightly, may depend on any crate, and no crate depends on it (#252;
`laptop.architect`, 2026-10-09T04:18Z,
https://github.com/synnaxlabs/foundation/pull/2099#issuecomment-6074164278).

Outside the binary: the Rust SDK `sdk` (`sdk/rust`, layer 4) builds on `program`,
`transport`, `sector`, `spec`, `env`, `types`, and `block`, and takes its clock,
entropy, network, the disk of a data directory (`os::stored`), and its shards from `os`
(#2923, #3212). The Python SDK `sdk-python` (`sdk/python`, layer 4, last) wraps `sdk`
with PyO3, parses keys and names with `types`, and maps the errors of `sector::local`
and the refusals of `program` to its texts. It is the module `foundation._native` of
the wheel `synnax-foundation` (`laptop.architect`, 2026-10-17T18:41:45Z,
https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238783320). The data
path of Python is `sdk` (C6, SDK DATA PATH; the crate, `laptop.architect`,
2026-10-15T17:56:55Z,
https://github.com/synnaxlabs/foundation/issues/2177#issuecomment-6198730147; its name,
`laptop.architect`, 2026-10-15T18:23:47Z,
https://github.com/synnaxlabs/foundation/pull/2724#issuecomment-6199257488; its
dependency on `access`, `laptop.architect`, 2026-10-15T18:05:02Z,
https://github.com/synnaxlabs/foundation/issues/2722#issuecomment-6198883707). Other
SDKs hand-write their data path against golden vectors (D12). Supersedes "the Rust SDK
reuses `block`, `types`, `codec`, and `wire`".
