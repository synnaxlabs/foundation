# Decisions

The source of truth for Foundation's design. Every locked decision, where each data
structure lives, the crate map, and what is still open.

How to read this record:

- IDs come from the design interview (A7, BQ6) or its labels (REGION LOCKED). Study
  decisions carry the study number (R9-D4, R12-4, R13-5). The studies are in
  `docs/research/`.
- "Supersedes" names an earlier entry that no longer holds. Section 1.16 lists every
  retired entry once.
- The person delegated these areas to the design session: quality, memory and
  performance, failover, names, delivery and wire internals, and "decide the best
  architecture". Decisions made under a delegation are as binding as the rest.
- A **region** is the part of the name tree that one voter set governs.
- To change a decision, follow "Interface changes" in `docs/coordination.md`. A change
  to a locked decision needs the person.

---

## 1. Current decisions by topic

### 1.1 Principles and lessons

- **D1, D7** Foundation is one Rust binary that moves data between devices, nodes, and
  stores. It is not a database: no historian, no query language, no long retention.
  Nodes keep a durable buffer (hours to days) for store-and-forward. The mesh is its
  own control plane; there is no separate service. Supersedes: D7 linked meshes, D7
  Raft library, D7 bootstrap peers in the file, D7 any node relays.
- **D2** Foundation is bidirectional. Connectors carry commands through the same device
  owner that reads. No control logic runs in Foundation. Commands expire at a deadline,
  are never replayed after an outage, and return an acknowledgment. Command authority
  and audit ship in the first release.
- **D3** Monetization is deferred. Product decisions are never shaped by what we sell.
  The node is free.
- **D6** People own contracts and oracles. Agents own all other code. Oracles are
  enforced by visibility (C9c). Supersedes: D6 path lock.
- **L1 (neutral model at the boundary)** At every boundary with interchangeable forms
  (syntaxes, carriers, time sources, secret stores, SDK languages), the core works on
  one neutral model and each form is an adapter. Ship one default adapter. Never shrink
  the model to the weakest form. A piece that only one form can express gets its own
  form-independent grammar.
- **L2 (library, not framework)** The code that has the edge cases owns its control
  flow and calls small shared components. Common cases get ready-made compositions
  built only from public parts. At every boundary, test the inverted shape first.
- **L3 (naming tell)** A compound name that repeats a responsibility means the package
  holds a second job. Split until the names get simple.
- **L4 (dependency direction)** Draw what each structure points at. Prefer settings as
  selectors over fields. Reuse a core concept before you add a side structure. Keep the
  model map current.
- **L5 (SEATING REQUIREMENT)** Each feature records the internals it touches, whether
  it can sit on the public surface (yes, partly, no), its boundary, and the trade.
- **SIMPLICITY DIRECTIVE (K5)** Keep the model and its vocabulary minimal. Express new
  needs with existing concepts (policies, selectors, channels) before new terms.
- **S1 principle** Never put in a frame or a series what both ends already know from
  definitions.
- **MODEL MAP rule** Anything that changes over time is a channel (quality, clock
  error, control state, acks, status), not a field or a side array.
- **REDUCTION rule** A policy never creates channels. Anything that creates a channel is
  an explicit definition.
- **BQ11b rule** A crate's value is the source of truth. A channel is a published copy
  for people, agents, and outside tools. Core decisions (failover, fencing) never read
  channels back. Upward flow goes only through values that the upper crate pulls.
- **LIBRARY RULE** Choose the architecture first, then a library or our own build, for
  every major dependency. A library that does its own I/O or reads the clock fails T1.
  Protocol cores (consensus, wire session, control gate, spec sync, clock offset) are
  sans-I/O state machines with thin drivers. Connectors are not.
- **CANONICAL LIBRARY RULE** Per protocol, prefer the canonical production-grade
  implementation in any language, statically compiled with our own Rust bindings, when
  its license allows static linking. Its threads stay inside the connector.
- **PERFORMANCE PRINCIPLES** Minimize copies, locks, and heap allocations at all costs.
  Own the memory and storage layout. No Arrow and no arrow-rs.
- **Root CLAUDE.md principles** apply to every crate: injected dependencies, no mutable
  globals, no load-time self-wiring, concrete types by default, fail loud on an internal
  dispatch key, no defense in depth.
- **Process** Each data structure and key decision is proposed with a sketch and
  locked only on agreement. RESCOPE: delivery and wire internals are tuned by
  benchmarks, not interviewed.

### 1.2 Data model

- **A0 + vocabulary** The unit of data is a channel. Identifiers are keys, never IDs.
  A sample is one value at one time. A series is an array of samples of one channel and
  one type. A frame holds series of several channels sent together. Never "batch".
- **A3** Names are dot-separated segments of letters, digits, `_`, and `-`.
  Case-sensitive; case-only collisions are rejected. `*` matches one segment, `**` any
  depth. The `@` prefix is reserved for Foundation. Hierarchy and struct fields share
  the dot. MQTT maps `.` to `/`. Letters are ASCII (decided by the person,
  2026-10-04). Widening to Unicode later stays backward compatible. `discover` maps
  non-ASCII device tags to ASCII names.
- **NAME LENGTH (2026-10-04)** A name or pattern holds at most 255 bytes. The person
  chose "255 bytes": it fits a one-byte length prefix, and raising it later stays
  backward compatible.
- **SPECIFICITY (#3)** Pattern specificity orders by more literal segments, then fewer
  `**`, then more `*`: `a.b` > `a.*` > `a.*.**` > `a.**` > `**`. A run of wildcards
  counts as its `*`s and one `**` (`a.**.*.**` is `a.*.**`). Two different patterns may
  tie (`a.*` and `*.a`); a tie between setting policies on one name is the S12 plan
  error. Access has no ties (X25).
- **A4 + M1/M2 answer** `channel::Key` is a UUIDv7 made with the channel. It is never
  reused and never changes. Files carry names only. The stored spec maps name to key,
  and `apply` assigns a key the first time a name appears. Renames are explicit
  (`foundation rename`), and `plan` shows them. UUIDs appear only in the stored spec,
  wire setup, and disk footers.
- **A5** A timestamp is i64 nanoseconds since the Unix epoch, UTC. The home rejects
  backwards samples and stamps near 1970 or far in the future (both limits are
  settings). Supersedes: A5 tie rule (by S6).
- **A6 (as revised by A8)** Late data is backfill on the same channel, labeled by the
  writer. Unlabeled backwards samples are rejected. Backfill is ordered and ends before
  the newest live sample. Live readers never see it; recording readers get it marked.
- **A7** An index channel holds timestamps. Each data channel points at one index.
  Alignment holds only inside one frame. A frame may carry any subset of an index's
  channels. A channel that names no index gets a private index. An index has one home
  and one writer in control at a time.
- **A8** Each index has one u64 `seq` over samples, with separate live and backfill
  counters, each gapless on its path. A restart continues from the disk. A home with a
  standby reserves seq blocks from voters. The codec sends seq only on jumps.
  Supersedes: A1 epoch and seq pair, A6 batch counting.
- **A9** Base types: bool, i8 to i64, u8 to u64, f32, f64, timestamp, duration, string
  (UTF-8), bytes, uuid. Numeric series are plain fixed-width arrays.
- **A11** Enums have a fixed integer type and an explicit value per variant. Samples
  store the integer. Unknown values pass through flagged. Variant names are not part of
  channel identity.
- **A12** Flags are an unsigned word with named bits and multi-bit fields. Named bits
  are addressable by dot. Samples store the raw word.
- **A13** Fixed arrays `T[N]` and `T[N][M]` (row-major) and bounded lists
  `list<T, max>`.
- **A16** Units only, no ranges. A unit lives where the number is defined: on a
  primitive channel or on a struct field. Foundation maps common units to the standard
  codes that targets need.
- **A17** Calculated channels are in scope. Calculations write data channels only,
  never commands. Scaling is a calculation. Raw counts plus calculated scaling is the
  default way to meet P1's byte target.
- **A19** No JSON type. `bytes` carries no label. Structure is expressed as a struct.
- **A20** No command channel type; a command is an ordinary channel. Its ack channel has
  the same type, and the connector writes the applied value to it. Failures go to the
  quality channel. Device state is a separate channel read back from the device, never
  an echo. Command and ack channels recorded together are the audit log. Supersedes:
  A20 channel retention (by S12), A20 quality codes (by S13).
- **S5** `Channel { key, name, kind: Kind }`, with
  `Kind::Index { error: Option<channel::Key>, control: Option<channel::Key> }` and
  `Kind::Data { index, quality: Option<channel::Key>, data_type, unit }`. No calculated
  or virtual flag.
- **S6** An index carries no placement, retention, or rate. Timestamps strictly
  increase per path. The clock error bound is a channel that the index points at with
  `error`.
- **S7 + QUALITY DECISIONS** A struct is a template of channels: one channel per field,
  all on one index. The runtime knows only primitive, fixed array, list, enum, flags,
  string, and bytes channels. Optional field presence is per frame (a presence mask over
  the writer's key set). Struct views are built from one frame, never from per-field
  latest values. Disk chunks record the seq ranges they cover. Supersedes: A10, A14
  validity bits, A15 struct fingerprints, S2 struct layout.
- **A15 (as revised by S7)** Types are defined as code. Enums and flags carry a
  fingerprint. A field added is a channel added; a field renamed is a channel renamed.
  Changes are sorted into safe and breaking before they take effect; breaking changes
  need explicit opt-in.
- **S11** The home's gate is the only control check. Unknown control state means "not
  in control". The home publishes control state on the channel the index points at,
  once per handoff. A writer asks for authority at open, capped by access. Authority 255
  cannot be taken. The control lease is an optional writer setting. There is no control
  policy.
- **S13 + BQ13** Quality is an ordinary channel of type `Quality` (OPC UA 32-bit status
  codes) that data channels point at. One quality channel can serve many channels. It
  may sit on its own index (written on change; a value holds until the next) or share
  its data's index (same writer, per sample, cheap through RLE). Sinks match quality by
  time (as-of). Supersedes: A18 side array.
- **S8 (identity part)** A node has a stable `node::Key` (UUIDv7) separate from its
  rotatable Ed25519 public key. No role fields. Anything that changes about a node is a
  channel under its name. Supersedes: S8 node as a spec definition (by BQ11a).
- **R9 type decisions (SETTLED BY ME)** R9-D1 per-entry types are interned once in the
  key set; R9-D2 bools are one byte; R9-D3 raw series are padded to element width and
  blocks are 64-byte aligned; R9-D4 variable-length series are `ends[n]` then data;
  R9-D5 `types` is byte layout only and meaning lives in the spec; R9-D6 `time::Span`
  value and `duration` keyword; R9-D7 JSON uses RFC 3339 UTC with 9 fraction digits,
  span unit strings, and keys as UUID strings, and never appears on the data path; R9-D8
  exact reduced-fraction `Rate` with u128 offset math; R9-D10 panic on internal overflow
  and checked math for outside values; R9-D13 `types` modules are time, sample, series,
  frame, channel, node, quality, name, hash (R16-7), authority (#57: `access`, `spec`,
  `wire`, and `control` all use it), and digest (the BLAKE3 address of spec chunks and
  blobs, which `spec`, `blob`, `wire`, and `mesh` share); R9-D14 checks run once, at the
  home. R9-D11 rejected (slots won). Supersedes: R9-D13 `block` module (by SRP PASS).
- **MODEL MAP (current)** Data channel -> index, -> quality (optional), -> data type,
  -> unit. Index -> error channel (optional), -> control channel (optional). Type ->
  other types; types never point at channels. Policies -> names through selectors;
  channels never point at policies. Readers and connectors -> channels by name or
  selector. Calculation -> inputs, -> its own output index. Connector -> channels it
  reads and writes; channels never point at connectors. Region -> name prefix. Node,
  connector, subject, and channel names share the one name tree.

### 1.3 Delivery

- **B1 (as revised by S10)** The home keeps a disk buffer for indexes whose retention
  keeps data. Data stays while a holding reader has not received it, within one disk
  budget per node. When the disk is full, the oldest data goes first, readers get an
  explicit gap, and the node warns early and names the reader that holds the buffer.
  Group commit every few ms. Complete readers get frames only after they are on disk.
- **S10 (reader)** A reader is a session, not a definition: `Reader { name:
  Option<String>, select: Selector, mode: complete or latest, from: now, oldest, seq,
  time, or resume, max_age, hold: Duration (default 0) }`. Only complete mode holds.
  A hold is capped by the index's retention. One session per named reader; a new one
  takes over. Out connectors carry reader settings in their config. Current readers and
  holds are published on status channels. Supersedes: B1 durable reader, B2 durable
  and ad-hoc readers.
- **S10 + S11 + BQ7 (writer)** A writer session is `{ subject, authority, control
  lease, path: live or backfill, channels, confirmation: stored or replicated }`.
- **B2** Selectors stay live: channels created later that match join the subscription.
  A start time maps to the first sample at or after it, per index. A range the buffer no
  longer has is an explicit gap.
- **B3** Complete mode orders per index only. A reader reports one cumulative position
  per index (a durable reader after it stores the data). Flow is push with
  reader-granted credits. A slow reader catches up from disk and never slows writers or
  other readers. Delivery is at-least-once; seq makes repeats easy to drop.
- **B4** Latest mode gives a new reader the current value at once. A slow reader keeps
  at most one waiting frame per index; a newer frame replaces it; frames never split. No
  replay after a disconnect. Frames go out before the disk sync.
- **B5** Live writes never wait. If the disk queue or the pool is full, the home records
  an explicit gap and warns. Backfill waits for room.
- **B6** One write call is one frame. Smart batching is the default. Catch-up may merge
  consecutive frames (limit in X30). Acquisition and transmission settings are code,
  changeable on a running mesh, with defaults chosen by the end-to-end sweep.
- **B7** A frame applies whole or not at all, per index. Live writes are never retried.
  Backfill frames carry numbers, and the home drops repeats. A writer may resend
  unconfirmed live data as backfill.
- **READ COPIES (delivery part)** `hub` merges latest subscriptions for one remote home
  into one upstream flow.
- **BQ3** `hub` is the whole layer-3 window: `reader()`, `writer()`, read-only
  `spec()`, `watch(selector)`, `now()`, and `block(len)`. Remote SDKs get the same
  surface over the network.
- **BQ4** Encode once at entry: the home encodes local writes, and the writer's `hub`
  encodes remote ones. Decode once at the reader. Each series decodes by itself. The
  home checks data series by their headers only and decodes index series.
- **RESCOPE** B1 to B7 are starting points tuned by T1 benchmarks.

### 1.4 Storage and codecs

- **S2 (as revised by M3, BQ4, R10)** On the wire, a series payload is only its data
  bytes. Type and width come from definitions. The codec choice is a 1-byte tag per
  vector inside the bytes. Supersedes: S2 series struct, S2 per-channel compression,
  S2 per-link re-encode.
- **S3 (R9-D4, r2 Q1)** Fixed-width series: `values[n]`. Fixed arrays: row-major.
  String, bytes, and lists: `ends[n]` (u32 end offsets) then data; lists nest. The
  sample count travels once per index group in the frame.
- **BQ4 + ADAPTIVE COMPRESSION (r10)** Per 1024-value vector, one stats pass gives exact
  sizes, and the encoder picks the smallest. Raw is always a candidate. The minimum
  saving is a fixed 1/8. No sampling and no hysteresis, except ALP's top-5 exponent
  pairs, refreshed about every 100 vectors. Codec set: integers raw, FFOR, delta
  (natural order), RLE; timestamps add stride; floats raw, ALP, fdelta, RLE; `max` mode
  adds pco. No zstd and no ALP_rd. Policy: `compression { select, mode = auto, raw, or
  max }`, default `auto`. The validator is the top fuzz target.
- **S4 (r2 starting point, not locked)** Per shard: a preallocated write-ahead ring
  (CRC32C per record, one group-commit sync), then immutable columnar segments with one
  chunk group per index. Eviction deletes whole segments. No per-channel files. A failed
  fsync is fatal and never retried.
  Ring record (starting point): `[len: u32][crc32c: u32][payload]`, one per group
  commit, starting on a 4096-byte boundary so a commit never rewrites a synced block.
  The CRC continues from the record before (a chain), and each open of the ring starts
  a chain from a random value, so bytes of an earlier chain never read as the next
  record.
- **BQ9** Re-index by changing `index` in the files. The old home seals the channel at
  its last accepted sample and records the seal with voters (which region: X39). The
  history "index A until T, index B from T" is runtime state in `mesh`; the spec keeps
  only the current index. Readers' `hub` joins the spans. No data moves. A connector
  rate change is not a re-index.
- **RE-INDEXING REQUIREMENT** A re-index keeps key and name. Stored data keeps its
  original timestamps. Readers see one continuous channel.

### 1.5 Memory and performance

- **P1** Targets: 100M numeric samples/s per node (disk buffer plus one complete
  reader); within 2x at 100k channels; latest-mode p99 under 250 us over one encrypted
  LAN hop; under 4 bytes per sample for typical sensor data, timestamps included;
  Raspberry Pi 4 with 1 GB: idle under 50 MB, start under 1 s. A regression over 5% on
  the dedicated machine blocks a merge.
- **M1** Node-local u32 `channel::Slot`s. Each writer session gets an interned key set
  (slots plus types, R9-D1). Frames point at the key set id. Supersedes: S1 frame
  struct.
- **M2** Readers get a view: the frame plus a mask cached per key set and reader. The
  home routes by key set.
- **M3** One pool block per frame: a header (key set id, presence mask, sample count
  per index group), one descriptor per series, and series bytes back to back. A series
  is a slice. One refcount per frame. Connectors write straight into `hub.block(len)`.
  Supersedes: S2 per-series buffer.
- **M4** Each shard owns a pool that `node` injects. A release returns the block to the
  owner shard. No global pool.
- **M5** Blocks hold offsets, never pointers.
- **MEMORY BOUNDS** A hard pool budget per node. Pools reserve address space, commit
  pages lazily, and purge after idle. Credits cap the blocks a reader can pin. A reader
  that falls behind is served from disk. When the pool is full, a live write records a
  gap and backfill waits.
- **R9-D9** Atomic refcount. `Unique` is writable; `Block` is immutable after freeze. No
  copy-on-write.
- **Performance rulebook** Rules 1 to 14 bind every implementing agent, the performance
  agent, and every adversarial reviewer.
- **C2 (working assumption)** One shard per core owns its indexes with no locks. Each
  shard runs one Tokio `LocalRuntime`. Vendor libraries run on dedicated threads. A
  connector's shard is chosen by its index (R12-8). The `memory` builder measures the
  handoff on Linux before this locks.

### 1.6 Time

- **C6 (as revised by R6 TIME LOCKED and TIME ADAPTERS)** Each node keeps a mesh clock:
  the OS clock plus a measured offset, with an error interval (earliest to latest).
  It never steers the OS clock unless that is opted in where privileged. All timestamps
  are in mesh time; connectors convert device time. Each node publishes
  `<node>.clock.offset` and `<node>.clock.error`. Zero time config by default: sources
  are detected, and the clock follows the smallest measured bound, with no fixed
  ranking. Supersedes: C6 fixed source choice (X36).
- **R6 TIME LOCKED** Own sans-I/O estimator over our transport. The bound is half the
  round trip. Keep the fastest exchange per source, combine sources, widen the bound
  with drift, slew only. Sources are read directly: mesh peers, GPS, PPS with NMEA, the
  NIC hardware clock (kept by ptp4l), and the OS daemon. No PTP client in v1. Device
  clock fitting (DAQmx, LabJack) is a connector-library component that writes residual
  error to the index's error channel.
- **TIME ADAPTERS** Neutral model `Measurement { at: local monotonic, offset, error }`.
  The estimator never knows what a source is. Each source is an adapter with its own
  loop. `node` builds the source table. Adapters probe for hardware and privileges. The
  same estimator serves device clocks in the connector library.
- **ESTIMATE COMBINE (2026-10-04)** A `Measurement` is about one local clock (the node's
  monotonic clock, or a device's sample clock in nanoseconds, #84): its offset is mesh
  time minus the local reading at `at`, and its error is a half-width from 0 to 36500
  days. A bound grows by the drift bound times the time from `at`, in both directions.
  The drift bound is at most 10%; `Drift::UNDISCIPLINED` is 200 ppm. A measured
  oscillator rate goes into `Drift` later, as an additive change. Each source keeps its
  last 8 measurements and offers the one with the smallest bound now. This reads R6 TIME
  LOCKED's "keep the fastest exchange" with drift: an old fast exchange loses to a fresh
  slower one. `combine` takes one `Filter` per source and returns the hull of the
  offsets inside the most bounds (Marzullo). It fails when no offset is inside more than
  half of them. This reads C6's "follows the smallest measured bound": when sources
  agree, the result is never wider than the narrowest. The result holds the true offset
  when the bounds that hold it are a majority and every other bound misses them. Decided
  by the `time` builder (#49). A device's measurements go to the oscillator fit (`Fit`),
  never to `combine`. Node sources keep `Filter`, not `Fit`: a network exchange puts the
  true offset at about the same place in each bracket, so an overlap gains little, and a
  broken drift bound would stay wrong for the life of a fit, not for 8 exchanges.
  Decided by the coordinator (#84).
- **BQ20** Wall time comes only from `clock`. Clippy `disallowed-methods` and the
  architecture agent enforce it.
- **R9-D13** The layer-2 crate is `clock`. `types::time` holds `Stamp`, `Span`, and
  `Range`. Supersedes: crate name `time`.
- **R9 keep list** `Stamp - Stamp = Span`; one format and parse grammar for spans,
  ranges, and ns ISO stamps.
- **TIME TEXT (#3)** A span is one number and one unit (`ns`, `us`, `ms`, `s`, `m`, `h`,
  `d`). Output uses the largest of `d`, `h`, `m` that divides the span, else the largest
  of `s`, `ms`, `us`, `ns` not more than the span, with a decimal fraction: `3d`, `90s`,
  `1.5s`, `250us`, `0s`. Input takes a decimal fraction and a leading `-` and rejects a
  value that is not a whole number of nanoseconds. A stamp is RFC 3339: output is UTC
  with nine fraction digits; input needs an offset, takes up to nine fraction digits,
  and rejects second 60. A range is the ISO 8601 interval `<start>/<end>`. A `Range`
  never ends before it starts (`Range::new` returns `None`), so its text always round
  trips; input rejects an end before the start.

### 1.7 Transport

- **TRANSPORT SHAPE LOCKED** One session model: prioritized, cancellable streams plus
  optional datagrams. Carriers are adapters: QUIC (noq-proto), TLS over TCP, relay (TLS
  on 443 through designated nodes), and a one-way diode carrier. An adapter that lacks a
  feature emulates it. Reliability is per delivery mode: commands reliable and highest;
  latest drops stale frames by cancel or datagram and keeps `TCP_NOTSENT_LOWAT` small
  over TCP; complete is reliable and ordered with credits; catch-up is lowest. The
  default carrier per traffic class comes from measurement.
- **T1 seam** Foundation's own `Transport` trait sits in front of every carrier.
  Amended by SIM NETWORK: the trait is private to `transport`, and `sim` replaces the
  network below the carriers (`env::net`), not the transport.
- **R5 starting points** Addresses come from the mesh, not DNS. A TCP path is
  mandatory. Try direct UDP, then direct TCP, then a relay. Relays admit only known
  keys. No n0 infrastructure. iroh `Endpoint` is rejected; quinn-proto is the fallback
  core. The diode carrier is UDP, Noise K, RaptorQ, seq, and codec keyframes: best
  effort with recorded gaps; commands, Raft, and clock exchange cannot cross it.
- **A4 (wire part)** Each connection swaps keys for short numbers.
- **STREAM DISPATCH (2026-10-04)** `transport` is blind to protocols. The first
  message of each stream, and each datagram, starts with a header from `wire` that
  names the protocol (`clock`, `mesh`, `replica`, `blob`, `hub`). `node` holds the
  table from protocol to handler and runs one accept loop per session. A protocol
  that the table does not know comes from a peer, so the loop resets that stream with
  a code and goes on. Decided by the coordinator (network's review of #53).
- **ONE PORT PER NODE (2026-10-04)** A node listens on one UDP port and one TCP port,
  however many shards it runs, so each site's firewall needs one known port per
  conduit. Each QUIC connection belongs to one shard, and every connection ID a node
  issues encodes that shard. A receive loop on one shard reads the UDP socket in
  batches and hands each batch to the owning shard over the C2 ring; every shard
  sends on the same socket. The TCP listener accepts and moves each stream to its
  shard. `env::net` therefore splits a UDP socket into a receive half with one owner
  and a send half that any shard may use, and `sim` models the split. Rejected: a
  port per shard (a port range in every firewall), kernel reuse-port hashing (routes
  by address, breaks on NAT rebinding), and one shard doing all network work. If the
  receive loop saturates on Linux, add a reuse-port group steered by the same
  connection ID. Decided by the design session under the architecture delegation
  (#53).
- **TLS RANDOMNESS (2026-10-04)** All randomness inside TLS (key shares, client
  random, nonces) comes from aws-lc, not from `env`. rustls holds its random source
  as a `&'static` value, and aws-lc makes X25519 key shares with its own randomness,
  so neither can be injected without a leak per `Transport`. It changes bytes, never
  sizes or timing. Simulated runs still replay because nothing branches on those
  bytes; replay traces leave out ciphertext and handshake randoms, and a `sim` test
  runs one value twice and compares the traces. The person accepted it ("Accept TLS
  RANDOMNESS"), from `network`'s proposal on #54.
- **R14** Do not build on Zenoh; a Zenoh connector may come later. Measure QUIC against
  TLS over TCP on Linux early.

### 1.8 Consensus, regions, and the spec

- **S9 (as revised by R4 SETTLED)** `State { spec, runtime }`. Only `apply` changes the
  spec (see X28); only the mesh changes runtime state. `plan` compares files with the
  spec only. Raft holds each region's spec pointer (version and root hash) and runtime
  state. Fast state (status, health, clock error, control state, reader positions)
  stays out of Raft. Supersedes: S9 name-hierarchy tree, S9 gossip hints.
- **R4 SETTLED** Own sans-I/O Raft modeled on etcd/raft, PreVote and CheckQuorum on,
  with etcd scenarios and the TLA+ spec as oracles. No gossip. Each region's spec is one
  prolly tree keyed by full name, about 4 KiB chunks, BLAKE3. Each change record lists
  its new chunks. A region's voters sit on one LAN. A node fetches only the regions and
  ranges it uses.
- **RAFT SURFACE (#5)** `raft::Raft::new(Config, Start)` builds a follower. `Config`
  holds the fixed inputs (key, tick counts). `Start` holds what the node had on disk:
  `hard` (term and vote), `voters`, and `last`, the last log position, which stands in
  for the log until replication lands. `Raft` takes `tick(random)`, `step(message)`,
  and `campaign()`, and gives `hard()` and `messages()`. The caller writes `hard()` to
  disk before it sends `messages()`, so a candidate counts its own vote at once.
  Randomness enters only through `tick`: a node draws its election timeout on the
  first tick after a reset. PreVote and CheckQuorum have no off switch. Until
  replication lands, a new leader announces itself with a heartbeat. A node that is
  not in its own voter list votes and follows, but never campaigns. `step` does not
  check that a sender is a voter (a voter can learn late that a peer joined), so the
  caller authenticates the sender and decides which nodes may send.
- **K5 + REGION LOCKED + K5 REVISION** There is one mesh. A region keeps changing its
  own definitions while cut off. A region changes its own voters. The parent only
  creates or removes a region, or forces a takeover (admin on the parent, `--force`,
  epoch bump; nodes reject commits from an old epoch). The parent cannot veto. Access
  across regions is ordinary access policy. A change that spans regions commits per
  region in dependency order. Supersedes: D7 linked meshes, K5 parent-owned voters.
- **REGION BLOCK (tunable syntax)** `region "site_a" { voters = [...] }` declares a
  region by name prefix. Regions nest like names. Supersedes: K5 voters policy.
- **r4 reconciliation (SETTLED BY ME)** Definition references (index, quality, error,
  control) stay inside one region. Placement is not a key reference (rules in 1.9).
- **VOCABULARY + REGION LOCKED** "Region" names the governed part of the tree. Docs say
  "cloud region" for providers. The old term for the governed part is retired, and the
  word "branch" is reserved for a possible future copy-on-write mesh branching feature.
- **BQ1** Layer-2 order. `hub` pulls incoming connections through `Transport::accept`.
  `blob` is one content-addressed store with peer fetch for spec chunks and upgrade
  binaries.
- **BQ2** `types` holds values. `spec` holds definitions, the tree, hashes, diffs, and
  the one resolver `spec::resolve`. A connector's config is an opaque document that only
  its kind decodes.
- **BQ11a** Joining is an operation. An admin creates a join ticket (single-use or
  reusable, with an expiry, scoped to a region and name prefix). The node joins with it,
  voters record membership, and the join is logged on the changes channel. Files name
  nodes only where they matter (voters, placement). Ephemeral nodes are removed after a
  set time offline. Tickets are secrets.
- **S9 (changes log)** A built-in changes channel carries the small change records; seq
  is the Raft log index; any copy can serve it; readers resume from any source. There
  is one per region (X29).

### 1.9 Replication and failover

- **A1 (current part)** One home per index orders samples, keeps the buffer, and runs
  the gate. Reads may come from a copy. Supersedes: A1 channel home field, A1 standby in
  the mesh file.
- **S12 (placement part) + B7** Placement is a policy: `placement { select, standby,
  copies }`. With no placement, an index's home is the node of the connector that writes
  it (precedence in X22).
- **BQ6** Asynchronous replication. The `replica` component ships each index's log
  (stored bytes, reader positions, control handoffs, dedup marks) without touching the
  write path. Takeover is the home's crash recovery plus one fence check, inside `home`.
  Supersedes: r8 Q6 standby as a reader.
- **BQ7** Live writes never wait. The home publishes stored and replicated marks per
  index. Each writer picks its confirmation and resends unconfirmed frames after
  failover, deduplicated. Voters promote the standby when the home's lease lapses,
  however far behind it is. The old home's tail returns as deduplicated backfill.
  Failback is manual. By default the home sends to the standby after its own sync (an
  SSD is advised for critical homes; a Pi SD card loses about 1.6 s per crash).
- **BQ8 (FAILOVER DELEGATED)** Each node holds one lease, from its own region's
  voters. An index's holder record lives in that region. The standby must be in the
  home node's region. The name's region governs the definition. Supersedes: S9 lease
  wording, r8 Q8 lease per group.
- **BQ10 (FAILOVER DELEGATED)** One placement covers a connector and every index under
  its name. The standby connector starts cold behind the same fence as the home's
  writes. Failback is a planned move.
- **READ COPIES** Placement `copies = [node]` keeps a never-promoted copy of an index,
  fed by `replica`, in any region. Remote readers read and hold at the copy, so a weak
  link carries each sample once.
- **FAILOVER DELEGATED** The remaining r13 choices are parameters tuned by simulation.

### 1.10 Connectors and calculations

- **D4** The connector list is a parameter. Adding one is cheap: one small contract, a
  conformance kit and a protocol simulator per connector, and one compiled-in kind table
  (C-backed kinds behind build flags). First acceptance scenario: a remote test site
  (NI, LabJack, PLC over Starlink or cellular) streams to cloud stores and laptops with
  no loss, synced clocks, and commands back.
- **C3 + C5 SHAPE** One concept, the connector. Integrations merged into it, and the
  word "integration" is retired. A connector is named in the tree and governed by its
  region. Its changing state is channels under its name. Shared error set: retry,
  config, device.
- **GROUPS DROPPED + CONNECTOR = TASK** A connector is a task: one index for reading
  (one rate, one clock, one writer) or one reader for writing. There are no groups.
  Connectors that name the same endpoint share it through a library component that owns
  the handle once per node. The kind's checker rejects combinations the hardware cannot
  do. Supersedes: C3 REFINEMENT groups.
- **Commandable parameters (same lock)** A kind declares its parameters and which can
  change at runtime. The connector config chooses which are commandable. Each
  commandable parameter is a channel with an ack (A20). Access and authority decide who
  sets it. Files give only its starting value. The library gives every kind `running`.
  Supersedes: r8 Q14 group run channels.
- **BQ5** Each kind owns `async fn run(&self, ctx: Context)`. The supervisor only starts
  and cancels it. `ctx` gives hub sessions, status, run commands, secrets, and cancel.
  `hub` and `home` enforce the rules. `connector` is a library of components plus
  ready-made compositions built only from public parts. Supersedes: r8 Q5 actor with
  device hooks.
- **C5 + KINDS OWN THEIR CONFIG** Each kind owns parse, check, discover, and run, built
  on shared components. `config` never knows a kind's fields. A kind returns diagnostics
  with positions plus the channels it reads and writes. Calculations are a kind. The
  engine (powerful, Arc-style, in Rust: rule-based filtering, waveforms, FFT) is a
  separate later design. Locked guarantees: data only through hub sessions, a plan-time
  check, resource isolation (own threads with a budget, or another node), determinism
  (time only from samples and ctx), and outputs on the calculation's own index.
  Supersedes: r3 single-expression language, r3 first-input index, r8 JSON Schema
  check in `config`.
- **BQ15** A set of devices that the driver acquires as one unit is one connector.
  Otherwise, separate connectors and indexes, never two writers.
- **R7 starting points** OPC UA: open62541 compiled in, with our own crypto plugin on
  aws-lc or compiled-in mbedTLS. Modbus, MQTT with Sparkplug B (pass the TCK), and Kafka
  (pure Rust on `kafka-protocol`, rdkafka behind a flag): built sans-I/O. DAQmx and LJM:
  runtime-loaded bindings, NI functions declared by hand. Codecs: built. Crypto: rustls
  with aws-lc-rs and blake3. Tooling: clap, schemars, toml_edit, tracing. Our own thin
  MCP server, Prometheus text output, and InfluxDB line protocol. FIPS build later.
- **REDUCTION** Deadband is a policy, `reduction { select, deadband }`, unit-checked,
  most specific wins. Connectors read it through a library component and pass it to
  devices that support it. Frames carry only channels that moved. Swinging door is a
  calculation with its own index. Raw and reduced data live side by side through
  retention.
- **QUARANTINE** An out connector that gets a permanent rejection moves the frame to its
  quarantine (a hold on the original data plus an error record) and moves on.
  Operations list, retry, and drop it. Its size is a status channel. It is a library
  component.
- **DEATH RECORDS** When a writer session ends without closing, the home writes a
  "source lost" quality sample. A clean close writes nothing. Scope: X19.
- **R12 catalog (proposal, partly adopted)** Components (cancel, pace, clock stamping,
  retry, endpoint, link, drive, thread, queue, cycle, status, run, out, calc align) and
  compositions (polled, clocked, pushed, cyclic, out, calc). The kind's `&self` holds
  process-lifetime parts that `node` injects; `ctx` holds one run's capabilities.
  Group-based parts need revision (X5).

### 1.11 Config as code

- **K1** The boundary is a syntax-neutral Document (blocks, attributes, values, and a
  source position on every value). Each syntax is a front end that reads and writes it:
  `config-hcl` first, HCL the default; YAML read-only until a format-preserving Rust
  editor exists. `node` builds the front-end table keyed by file extension. Files hold
  data only (no loops, variables, or modules). SDK code may produce a Document directly.
  Never shrink the model to the weakest syntax. Supersedes: r3 plain HCL.
- **K2 (tunable)** The core knows only full names and regions. `plan` groups changes by
  region. One directory per region is the default layout that `init`, `discover`, and
  `export` write; `plan` warns on a mismatch. Full names everywhere, no imports.
  `discover` proposes diffs and never overwrites.
- **K3** Files in Git are the desired mesh; the spec is the running mesh. Commands:
  `discover`, `plan`, `apply <plan>` (commits exactly the reviewed plan; refuses if the
  spec changed; compare-and-swap on the pointer), `explain`, and `export`. Each has
  `--json` with stable change kinds.
- **A2** The files list every channel. Hardware channels enter through `discover`. A
  file may open a name folder to one program, where the first write creates the
  channel; `plan` lists such channels until they are added to files or deleted.
  Mechanism: X28.
- **S12 + C8 amendment** Settings are policies that select names. One `Selector`
  (patterns with `*`, `**`, and `!` exclusions) serves subscriptions, readers,
  connectors, policies, and access. Most specific pattern wins; equal specificity is a
  plan error; `explain` shows each effective value and its source. A rename can move a
  channel under other policies, and `plan` shows it. Current policy kinds: retention,
  placement, transmission, compression, reduction, time, access, and secret store.
  Targets and combination rules: X25, X26. Specificity: SPECIFICITY (#3).

### 1.12 Access, identity, and secrets

- **C8** A subject is anything that reads or writes (person, agent, program,
  connector), named in the tree and governed by its region. People, agents, and
  programs authenticate with keys; a node vouches for its connectors. Access is
  allow-only, default deny, with no conflicts: `access { subjects, select, allow,
  authority }`. Actions: read, write, plan, apply, secret, admin. No groups or roles; a
  group is a selector over subject names. A connector may write channels under its own
  name by default. `plan` lists access changes separately. SSO comes later.
- **K4** Config refers to secrets by name only. Values never appear in files, plans, or
  output. Secrets are write-only (`secret set`, `secret delete`). `plan` checks that
  every reference resolves. Agents wire references but never see values.
- **BQ16** A secret value is encrypted to each eligible node (the placement's nodes,
  standbys included). Voters store ciphertexts outside the spec. Only `secret set` and
  `secret delete` change them. On a key rotation or a new standby, a node that can
  decrypt re-encrypts. `discover` takes credentials for one call only.
- **SECRET STORES AS ADAPTERS** `ctx.secret(name)` is the only path for kinds. Adapters:
  the built-in sealed store (default, works offline), a node environment variable or
  file, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, GCP Secret Manager, and
  Kubernetes Secrets. A policy picks the store per name. External adapters authenticate
  with the node key and may cache values sealed to it (this delays revocation).
- **BQ12 (locked 2026-10-04)** Identity across forwarding nodes. The person adopted
  r15 in full (sections 4 and 6). A remote subject signs a short-lived hello and each
  session open, the gateway forwards the signatures, and the owner verifies them
  against the subject's keys in the spec. A node acts only as itself or as connectors
  placed on it. Node-to-node traffic is authorized by role. r15 decisions 2 to 10 hold
  as written there (session opens signed, a key list per subject in OpenSSH format,
  `apply` signs the plan hash, every node checks every change record, audit records
  the subject and the forwarding node, MCP runs beside the agent, the caller seals
  secret values, no end-to-end frame integrity in v1).

### 1.13 Operations, agents, and the factory

- **Factory constraint** Two people, each on an individual Max plan. The factory runs in
  attended, locally started sessions, not as an unattended daemon.
- **BENCH SPEND (2026-10-04)** Linux benchmarks that need real machines run on rented
  AWS machines. The person: "you're welcome to provision AWS machines. SET STRICT COST
  LIMITS. I don't want more than $100 spent". The limit is 100 USD in total, across
  all benchmarks, until the person raises it. Only the coordinator provisions, by the
  procedure in `docs/coordination.md`.
- **C7** One operation table (typed input and output, error codes, read-only and
  destructive flags) generates the CLI (`--json`), annotated MCP tools, and docs
  embedded in the binary. Every error has a stable code and a fix-it hint. Status is
  channels; there is no status API. `foundation init` writes an agent guide.
  `plan --simulate` runs a plan on a simulated mesh.
- **C7 + D12 rejected** Each SDK has a generated base layer and a hand-written native
  layer. Each SDK's data path is hand-written as the fastest idiomatic code for its
  language. Guard: the Rust codec is the reference; golden vectors from it run in every
  SDK's CI; differential fuzzing runs both ways; the drift agent watches it.
- **D5** Developers extend Foundation through SDKs (Python and Rust first), not in-node
  plugins. A plugin system is open.
- **BQ17** The `ops` crate holds the operation table and generates the CLI, MCP tools,
  and docs.
- **BQ19** `sim` ships in the binary behind a feature that product builds turn on. Its
  size is measured against P1.
- **BQ11b** `node` pulls each crate's values and writes status channels through `hub`.
  Rebalancing is an outside controller that reads status channels and acts through
  plan and apply.
- **OWN REPO (revises C9a)** Foundation lives in its own private repository,
  `synnaxlabs/foundation`, with one Cargo workspace: `crates/` (crate list in section
  4), `xtask/`, `oracles/`, and later `sdk/` and `bench/`. Every PR runs the layer
  check (`cargo xtask layers`).
- **MULTI-SESSION FACTORY** One coordinator session and several builder sessions work
  at once, each builder in its own worktree. Work is tracked in GitHub issues and PRs.
  Sessions message each other with `SendMessage`, but records live in the repo. Builders
  run under `/goal`; the coordinator runs `/loop /coordinate`. Details:
  `docs/coordination.md`.
- **MODELS** Fable 5.1 for the `memory`, `consensus`, and `storage` builders and for
  reviewers of `raft`, `mesh`, `block`, `ring`, `buffer`, crash recovery, lock-free
  code, and wake protocols. Opus 5.5 for the coordinator, the other builders, and other
  reviewers. Sonnet 5.5 for mechanical work and the code quality and drift crew agents.
  Sessions compact at 300k tokens of context.
- **NINE BUILDERS (2026-10-04)** The person approved five more builders (advisor
  brief): `write-path`, `storage` (Fable), `time`, `config`, and `network`. Builders
  file the issues for their own crates; the coordinator keeps interfaces, decisions,
  and the merge queue. Ownership: `docs/coordination.md`.
- **C9b** Work loop: a planning session splits a phase into tasks that own crates
  (amended by NINE BUILDERS: each builder splits its own phase); one agent per task in
  its own worktree; machine gates (build, lints, layer and stand-alone checks, unit and
  property tests, thousands of simulation runs, short fuzz, the 5% benchmark gate,
  mutation testing on the diff); two fresh adversarial reviewers; a person reads and
  merges; cleanup agents follow.
- **C9b2** A quality crew of six single-job agents (code quality, tests, architecture,
  performance, failure triage, drift), each with a person-owned rulebook. One command
  starts the daily run.
- **C9c** Oracles are enforced by visibility. A script writes an oracle section at the
  top of each PR summary and flags weakening. Each flagged change gets its own
  adversarial reviewer. A person merges every PR. Supersedes: T2 enforcement level.
- **AGENT REQUIREMENT** Every task must be easy to do with agents. C7 carries it.
- **R16-1 (2026-10-04)** Release builds keep integer overflow checks
  (`overflow-checks = true`), so R9-D10 holds in release too. An intended wrap uses
  `wrapping_*`. The 5% gate measures the cost. Decided by the advisor under the
  quality delegation.
- **R16-2 (2026-10-04)** Release builds set `panic = "abort"`. A broken invariant
  crashes the node, and crash recovery restarts it. Tests keep unwinding. Confirmed by
  the person on 2026-10-04.
- **R16-3 (2026-10-04)** A lint exception is `#[expect(lint, reason = "...")]`, never
  `#[allow]`. Clippy denies `allow_attributes`. Decided by the advisor under the
  quality delegation.
- **R16-4 (2026-10-04)** The workspace turns on the r16 lints that check rules the
  repo already has: `Debug` on public types, one path per item, one unsafe operation
  per block, exact error assertions, no discarded results, determinism, bounded
  loops, and names. The lists are in the root `Cargo.toml` and `clippy.toml`;
  `docs/claude/rust.md` gives the rules. Decided by the advisor under the quality
  delegation.
- **R16-5 (2026-10-04)** The strict decoder lints (`indexing_slicing`,
  `arithmetic_side_effects`, `as_conversions`, `string_slice`) apply only in crates
  that decode outside input: `codec`, `wire`, `document`, `config-hcl`, and each
  protocol parser in a `connector-<kind>`. Layer 1 decision crates (`raft`,
  `control`, `delivery`, `access`, `estimate`) deny `wildcard_enum_match_arm`.
  Decided by the advisor under the quality delegation.
- **R16-6 (2026-10-04)** Each crate keeps one public `Error` enum, or one per
  sub-boundary module. Microsoft's canonical error structs and
  `clippy::error_impl_error` are rejected: an enum lets a test pin the variant, and
  backtrace capture costs time on hot paths. Decided by the advisor under the quality
  delegation.

### 1.14 Testing

- **TESTING IS BEDROCK + T1** Injection rule: every component gets clock, network, disk,
  and randomness as inputs. Layers: (1) unit and property tests on every commit; (2)
  coverage-guided fuzzing of every decoder of outside input, short per merge and
  continuous nightly, crashes kept as regression inputs; (3) deterministic simulation of
  a whole mesh, thousands of runs per merge and millions nightly; (4) unit benchmarks
  and (5) component benchmarks on the dedicated machine with the 5% gate; (6) end-to-end
  performance against P1 on shared infrastructure, nightly and per release; (7) a
  protocol simulator per connector on every merge; (8) Synnax HITL runners with real NI,
  LabJack, and PLC hardware, nightly and per release. Mutation testing runs on the diff
  (C9b).
- **T2** Oracles are person-owned: simulation invariants, P1 targets and baselines,
  conformance suites, fuzz inputs (agents add, never remove). Agents write most tests.
- **CANONICAL LIBRARY RULE (testing part)** Protocol simulators and HITL test C-backed
  connectors. Simulation replaces any connector through `hub`.
- **R13 invariants (oracles)** The eight invariants in r13 section 9 become simulation
  invariants in `oracles/invariants/`.
- **R16-7 (2026-10-04)** `clippy.toml` bans `std::collections::HashMap`, `HashSet`,
  and `std::hash::RandomState`. Code uses `types::hash::Map` and `Set`, which have a
  fixed hasher, so a simulated run replays. A map keyed by outside input will get a
  keyed hasher with its key from `env` randomness. Decided by the advisor under the
  quality delegation.
- **R16-8 (2026-10-04)** `thread_local!` state is banned like every other mutable
  global. `clippy.toml` denies the macro. Decided by the advisor under the quality
  delegation.
- **R16-9 (2026-10-04)** Miri and cargo-fuzz run on one pinned nightly toolchain that
  only those gates use. The workspace toolchain stays stable. Decided by the advisor
  under the quality delegation.
- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. `clock::Clock`: monotonic time as
  `types::time::Monotonic`, and a `Sleep` future that resets without an allocation.
  `wall::Wall`: the OS wall clock, which only `clock` reads (a lint).
  `entropy::Entropy`: random bytes from the OS, or from the run's seed in simulation.
  `rng::Rng` is concrete (xoshiro256++ seeded from `Entropy`), so simulation replays it.
  `shards::Shards`, held only by `node`: the core count, and one thread per shard with
  its own executor. `tasks::Tasks`: spawns `!Send` tasks on the current shard.
  `threads::Threads`: dedicated threads for blocking code. Each runs one future, and it
  waits for an event only by awaiting a future, so simulation controls every wait. A
  lint denies the std blocking waits (`park`, `Condvar`, `Barrier`, `mpsc` receive).
  When a shard's main future completes, the shard drops its other tasks. A panic in any
  task ends its shard, and its `Handle::join` returns `Error::Panicked`. A dropped
  `Handle` would leave its thread running, so it is `#[must_use]`. On `os`, a shard is a
  Tokio `LocalRuntime` and `spawn_local` runs `Tasks`; on `sim`, the deterministic
  scheduler runs them. No other crate calls Tokio's timers or spawn. `env::files`
  (#37) gives files under one data directory, with owned blocks and a sync that
  poisons the file on failure (S4). `env::net` (#44) gives UDP sockets that move GSO
  and GRO batches with ECN and the local address, TCP streams, and listeners.
- **SIM NETWORK (2026-10-04)** `sim` replaces only the network, not the transport.
  The production carriers (QUIC through `noq-proto`, TLS over TCP, relays) run
  unchanged under simulation, which is why r5 rejected iroh. The network seam lives
  in `env` (`env::net`): `os` implements real sockets, and `sim` implements the
  simulated network with loss, delay, reorder, duplication, and partitions. `sim` does
  not depend on `transport`. `transport` owns the carriers and the session model, and
  its `Transport` trait is private. `Clock::epoch` gives the `Instant` at
  `Monotonic(0)` for libraries that take a std `Instant`. Decided by the design
  session under the architecture delegation.
- **BLOCK MEMORY (2026-10-04)** A `block::Pool` gets its address space through
  `block::Memory`, a small `unsafe` trait in `block`, because `block` sits below
  `env`. `os` implements it over `mmap` (reserve, commit, purge); `block::Heap`
  implements it over `std::alloc` for tests, Miri, and `sim`. `block` makes no OS
  call. `reclaim` takes back returned blocks on each loop turn; `purge` gives idle
  pages back on a timer that the shard owns (#2).
- **COUNTING ALLOCATOR (2026-10-04)** The person allowed one exception to "no mutable
  globals": "Allow in test binaries". A test or benchmark binary may hold one
  counting `#[global_allocator]` `static` with an atomic count, because Rust has no
  other way to count allocations. Never in a library or the `node` binary. The
  `xtask globals` check allows only this case.
- **ARM RUNNER (2026-10-04)** CI runs every test on aarch64 too, because a wake protocol
  can pass on x86 and fail on ARM (r11 4.1). The person chose "AWS runner always on" and
  said "I have tons of AWS credits". Three runners (`foundation-arm-a`, `-b`, `-c`)
  share one AWS m7g.2xlarge (8 vCPU, 32 GiB) in us-east-1 with no inbound ports, tagged
  `project=foundation-ci`, outside BENCH SPEND. One runner queued 9 runs while its host
  used about 30% CPU, so the person asked: "can we have multiple runners on a single
  machine?" ARM skips docs-only changes. The coordinator owns it.

### 1.15 Releases

- **C9d** Releases follow RFC 0058: dispatch from main or `release/X.Y`; tag `vX.Y.Z` in
  this repository; `-rc.N` never counts as shipped. One binary per target (Linux x86-64
  and ARM, macOS, Windows), each tested on real hardware. `foundation upgrade <version>`
  is an ordinary operation, rolling one node at a time; nodes fetch the signed binary by
  hash from a nearby peer. Each wire and disk format has one integer version; a node
  reads its own and the previous one; new formats turn on only after every node runs the
  release. Compatibility is owed only to stable releases.
- **BQ18** The desired version is in the spec. Nodes report versions with lease
  renewals. A rollout lock upgrades one node at a time. Finalize when all report.
  Multi-region scope: 5.1.

### 1.16 Retired entries

| Retired entry | Replaced by |
| --- | --- |
| A1 sketch: channel `home` field, epoch and seq pair, standby in the mesh file | S5, S12, A8 |
| A1 "control is a lease" (for every holder) | S11 (optional writer setting) |
| A2 and A15 "mesh file" and placeholder commands | K1, K3 |
| A5 tie rule (ties ordered by seq) | S6 strict increase |
| A6 seq counts batches | A8 |
| A10 columnar struct series | S7 |
| A14 validity bits | S7 + QUALITY DECISIONS |
| A15 struct fingerprints | S7 |
| A18 quality side array | S13, BQ13 |
| A20 channel retention, quality codes on acks | S12, S13 |
| B1 durable reader, B2 durable and ad-hoc readers | S10 |
| C1 and C9a crate lists | Section 4 |
| C3 REFINEMENT groups | GROUPS DROPPED |
| C4 integration contract | C3 |
| C6 fixed time source | R6 TIME LOCKED, TIME ADAPTERS |
| D6 path lock for agents | T2, C9c |
| D7 linked meshes, Raft library, bootstrap peers in the file, any node relays | K5, R4 SETTLED, BQ11a, TRANSPORT SHAPE |
| K5 voters policy with a selector, parent-owned voters | REGION BLOCK, K5 REVISION |
| r3 "one language", single-expression calculations, first-input index | K1, C5 + KINDS OWN |
| r8 Q5 actor with device hooks | BQ5 |
| r8 Q6 standby as a reader, positions as a channel | BQ6 |
| r8 Q8 lease per group | BQ8 |
| r8 Q11 `connector-status` kind | BQ11b |
| r8 Q14 group run channels and `stopped` | Commandable parameters (`running`) |
| r8 section 1.14 JSON Schema check in `config` | KINDS OWN THEIR CONFIG |
| S1 frame struct; S2 series struct, struct layout, per-channel compression, per-link re-encode | M1, M3, S7, R10, BQ4 |
| S8 node as a spec definition | BQ11a |
| S9 name-hierarchy tree, gossip hints | R4 SETTLED |
| T2 enforcement level as a parameter | C9c |
| Crate name `time` | `clock` (R9-D13) |
| HOME SPLIT placement of `control` and `delivery` in layer 2 | SRP PASS layer-1 rule (X17) |
| The old term for a region | "region" (REGION LOCKED) |

---

## 2. Where things are defined

Storage classes used in the table:

- **Files**: definition files, read through the Document model.
- **Spec**: the stored spec. One prolly tree per region in `blob`; the pointer (version
  and root hash) is in that region's Raft state.
- **Region state**: runtime state agreed by one region's voters through Raft, outside
  the spec.
- **Index log**: records in an index's log in `buffer` at the home, copied by `replica`.
- **Channel**: values published as samples.
- **Memory**: in memory on one node; not durable.
- **Node-local**: on one node's disk or local config; not agreed.
- **Kind-owned**: an opaque Document inside the spec that only the kind decodes.
- **Binary**: compiled into the binary.

### 2.1 Definitions

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Channel | Files, then Spec as `spec::Channel { key, name, kind }`. Sources of channels: X33 | People or agents in files; `discover` and `export` write files; `apply` commits | Every node through its spec snapshot; `home`, `hub`; kinds through `hub.spec()` | `spec` (type), `config` (check), `mesh` (commit) |
| Index | Spec: `Kind::Index { error, control }`. Its settings come only from policies | As channel | `home`, `delivery`, `hub`, `buffer` | `spec` |
| Data channel | Spec: `Kind::Data { index, quality, data_type, unit }`. The `index` edge is defined here only (X23) | As channel | As index | `spec` |
| `channel::Key` | Spec (name to key map), wire setup, disk footers. Never in files | `apply`, the first time a name appears | Everyone | `types` (value), `mesh` (assignment) |
| `node::Key` | Region state (membership record) | Voters at join | `hub`, `mesh`, `access` | `types` (value), `mesh` |
| `channel::Slot` | Memory, node-wide; never on the wire or disk | The node's interner when the node learns a channel (owner: X42) | `hub`, `home`, `delivery` | `types` (value) |
| Key set | Memory, one per writer session: sorted slots plus per-entry types | The interner at writer open | `home` (routing), `delivery` (masks), `hub` | `types::frame` |
| Per-connection short numbers | Memory, per connection | The `wire` encoder at setup | The `wire` decoder | `wire` |
| Data type | Spec, on each data channel (byte layout); interned per key set in memory | Files, then `apply` | `codec`, home checks, SDKs | `types` (layout), `spec` (meaning) |
| Enum and flags definitions | Files, then Spec as named types with fingerprints | People, `discover` | Sinks, SDK code generation, `plan` | `spec` |
| Struct template | Files. `config` expands it into one channel per field. Stored form is open (5.1) | People, `discover` | `config` (expand, plan), SDK code generation, `export` | `spec`, `config` |
| Unit | Files, on a primitive channel or a struct field; Spec on the data channel. The unit table and standard codes are in the binary | People, `discover` | Unit checks at plan, sinks, reduction checks | `spec` (`spec::unit`) |
| Quality channel | Spec: a data channel of type `Quality` that data channels point at (`Data.quality`). Own index or the data's index | Values: the writer of its index; the home writes death records (X19) | Sinks (as-of), calculations | `spec`; values through `home` |
| Error channel | Spec: `Index.error` pointer | Values: the connector that writes the index (clock fit residual plus mesh bound) | Readers, sinks | `spec` |
| Control channel | Spec: `Index.control` pointer, placed with its index | Values: only the home, one sample per handoff (a published copy) | People, agents, auditors, new subscribers | `spec`; values through `home` |
| Region | Files: `region "<prefix>" { voters }`. The parent's spec holds the delegation record `{ prefix, epoch, initial voters }`; the region's own Raft config holds current voters (X3) | Parent voters create, remove, or force takeover; the region changes its own voters | `mesh`, `plan`, every node | `spec` (definition), `mesh` (groups) |
| Voters | Desired: the region block. Actual: Raft membership of the region's group | The region's own commits (joint consensus) | `raft`, `mesh` | `mesh`, `raft` |
| Policies (all kinds) | Files, then Spec | People, agents | `spec::resolve` (settings) or `access` (access) | `spec`, `access` |
| Retention policy | Spec; selects indexes | Files | `delivery` (floor), `buffer` (trim through `set_floor`) | `spec` |
| Placement policy | Spec; selects connectors and indexes: `{ select, standby, copies }` | Files | `mesh`, supervisor, `replica`, `plan` | `spec` |
| Transmission policy | Spec; selects indexes (link side open, 5.1) | Files | `transport`, `hub` | `spec` |
| Compression policy | Spec; selects indexes; `mode` auto, raw, or max. The actual codec is a 1-byte tag per vector in the encoded bytes | Files | `codec` at the encoder (the home, or the writer's `hub`) | `spec`, `codec` |
| Reduction policy | Spec; selects data channels; deadband checked against the channel's unit | Files | Connector library component through `hub.spec()` | `spec`, `connector` |
| Time policy | Spec; selects node names; lists candidate peer nodes (default: the region's voters) | Files | `clock` | `spec`, `clock` |
| Access policy | Spec; `{ subjects, select, allow, authority }` | Files | `access`, called by the owners (`home`, `mesh`) | `spec`, `access` |
| Secret store policy | Spec; selects secret names | Files | The secret resolver | `spec` |
| Connector | Files, then Spec as `spec::Connector { name, kind, node, config }` | People, `discover` | Supervisor on the placed node, the kind | `spec` (shell) |
| Kind config | Kind-owned: an opaque Document in the spec (canonical form, no source positions, so hashes stay stable) | Files | The kind's check at plan, `ctx.config()` at run | `connector-<kind>` |
| Calculation | A connector of kind `calc`; program text is kind-owned; outputs on its own index | Files | `connector-calc` | `connector-calc` |
| Open folder (A2) | Files, then Spec (mechanism: X28) | People | `hub`, `mesh` | `spec`, `mesh` |

### 2.2 Runtime agreed state (region state)

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Node | Region state: membership record `{ key, name, public key, version, ephemeral expiry }` in the region that holds the node's name. Private key: node-local. Files only name nodes | Voters at join (ticket); removal operation; ephemeral expiry | `mesh`, `hub` (authentication), `access`, `plan` (name checks) | `mesh` (record), `node` (key material) |
| Membership | Region state: node records plus each region's voter set | Voters | Everyone | `mesh` |
| Node lease | Region state of the node's own region | The node renews; a renewal carries its version and seq block requests | Voters (promotion), `home` (fence, with the clock bound) | `mesh`, `home` |
| Actual home of an index | Region state of the home node's region: `{ home node, holder, seq block }` | Voters (promotion), `apply` (planned moves) | `hub` routing through `mesh` watches | `mesh` |
| Seq blocks | Region state of the home node's region | The home, through lease renewals | A new home after promotion | `mesh` |
| Index history (re-index) | Region state: spans and seals. The spec keeps only the current index. Which region: X39 | The old home proposes the seal; voters seal at lease end if it is down | `hub` joins spans for readers | `mesh` |
| Secret ciphertexts | Region state, outside the spec, one per eligible node (region of the secret: X40) | `secret set` and `secret delete` (sealing in `ops`) | The node that runs the connector decrypts | `mesh` (record), `ops` (seal) |
| Join ticket record | Region state: options and use count. The ticket itself is a secret, never in files | Admin through `ops` | Voters at join | `mesh`, `ops` |
| Delegation record | The parent region's spec: `{ prefix, epoch, initial voters }` | Parent voters | Nodes (epoch fencing) | `mesh` |
| Spec pointer | Region state: `{ version, root hash }` | `apply` (compare-and-swap) | Every node that follows the region | `mesh` |
| Spec tree | Prolly tree chunks in the `blob` store on each node's disk | `apply` writes chunks | Nodes fetch the ranges they use | `spec` (tree), `blob` (chunks) |
| Changes channel | The region's Raft log presented as a channel; seq is the log index; one per region (X29) | Voters | Any node, `plan`, agents | `mesh` (served through `hub`) |
| Desired version, rollout lock, format flag | Desired version in the spec; lock and flag in region state (multi-region scope: 5.1) | `ops upgrade`; voters | `node` (binary swap); `codec`, `wire`, `buffer` get the flag injected | `mesh`, `ops`, `node` |

### 2.3 Per-index state at the home

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Encoded samples | Index log (write-ahead ring, then segments) | `home` through `buffer.append`; `replica` through `buffer.append_at` | Complete readers (catch-up), `replica`, crash recovery | `buffer` |
| Seq counters (live, backfill) | Memory at the home; durable through the index log | `home` | `delivery`, `wire` (prediction) | `home` |
| Control state | Memory in `control` at the home; handoff records in the index log (truth, copied by `replica`); control channel (published copy) | `control` decides, `home` records | New home at takeover (from the log, X18) | `control`, `home` |
| Control lease | A writer session setting; state in `control` | The writer at open | `control` | `control` |
| Reader positions | Truth: `delivery` state at the home, written as index log records and copied by `replica`. A connected reader's `hub` keeps its own position. Status channels publish copies | `delivery`; `replica` copies; `node` publishes | `home` after failover; `hub` on resume | `delivery`, `buffer`, `replica` |
| Holds and floors | `delivery` (hold per reader and index); floor = f(holds, retention), handed to `buffer.set_floor` | `delivery` | `buffer` | `delivery`, `buffer` |
| Backfill dedup marks | Index log records | `home` | `replica`, a new home | `home` |
| Gaps | Index log records (explicit gap with a count) | `home`, `buffer` | Complete readers | `home`, `buffer` |
| Stored and replicated marks | Memory at the home (the replicated mark is the standby's position in `delivery`); published on status channels | `home`, `delivery` | Writers (confirmation), `node` collector | `home`, `delivery` |
| Latest mailbox | Memory: depth 1 per latest reader per index | `delivery` | The reader session | `delivery` |
| Credits | Memory per reader per index; credit messages on the wire | The reader's `hub` grants | `delivery` | `delivery`, `wire` |
| Masks and routes | Memory: mask per key set and reader; route per key set | `delivery` | The home's fan-out | `delivery` |
| Death records | Quality channel samples (X19) | `home` | Sinks | `home` |
| Read copy data | The copy node's index log | `replica` | The copy's readers, served by `home` in copy mode (X43) | `replica`, `home` |

### 2.4 Sessions and values in memory

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Frame | Memory: one pool block with a header (key set id, presence mask, sample count and seq per index group, X8) and descriptors `{ offset, len }`. Wire form per connection. Never stored as a frame on disk | Writers, through `hub.block` or the frame builder | `delivery` views, `hub`, `codec` | `types` (layout), `block` (memory) |
| Series | Memory: a slice of the frame's block. Encoded: tagged 1024-value vectors | Writers; `codec` | Readers | `types`, `codec` |
| Block | Memory: per-shard pools that `node` injects | Writers fill a `Unique`, then freeze it | Every holder, by refcount | `block` |
| View | Memory: frame plus mask | `delivery` | The reader session | `types` (value), `delivery` |
| Reader session | Memory: `hub` session (selector expansion, max-age check); per-index state in `delivery` at the home or copy | The reader (SDK, CLI, connector) | `hub`, `delivery` | `hub`, `delivery` |
| Writer session | Memory: `hub` session (key set, routing, confirmation); gate in `control`; seq and dedup in `home` | The writer | `hub`, `control`, `home` | `hub`, `control`, `home` |
| Subscription | The selector of a reader session, kept live in `hub` against `mesh` watches | The reader | `hub` | `hub` |
| Effective settings | Memory: a per-node cache of `spec::resolve` results | `mesh` | `home`, `transport`, `clock`, supervisor | `mesh` |
| Document | Memory: made by a front end from files, or by SDK code | Front ends | `config`, kinds | `document` (X21) |
| Selector | A value inside policies, readers, connectors, and access | Files, sessions | Every matcher | `types` (one matcher) |
| Plan | A JSON artifact with stable change kinds | `ops plan` | `ops apply` (commits exactly it) | `config`, `ops` |

### 2.5 Connectors, time, status, and node-local state

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Kind | Binary: one literal table built in `node` | The build | `config` (check), `ops` (discover), supervisor (run) | `connector` (contract), `connector-<kind>` |
| Commandable parameter | The kind declares it; the connector config marks it commandable; value = a channel plus an ack under the connector's name; files give the starting value (index layout: X24) | Subjects with access and authority | The kind, through `ctx` | `connector` (library), the kind |
| Run state | The `running` commandable parameter | As above | As above | `connector` |
| Shared endpoint | Memory: an endpoint registry on the kind value (process lifetime) | The kind | Connectors of that kind on the node | `connector` (endpoint component) |
| Connector status channels | Channels under the connector's name | The kind through `ctx.status()` | People, agents, tools | `connector` |
| Node status channels | Channels under the node's name (definitions: X27) | `node`'s collector through `hub`, from each crate's pulled values | People, agents, tools, rebalancers | `node` |
| Quarantine | Per out connector: a hold on the original data plus an error record (samples on a channel under the connector's name); size on a status channel | The kind, through a library component | `ops` list, retry, drop | `connector` |
| Secret value | Never in files, plans, or output. Built-in store: region state ciphertexts. External stores through adapters. References by name in kind config | `secret set` (person or CI) | `ctx.secret()` on the connector's node | `ops` (seal), `node` (decrypt), resolver (X40) |
| Time sources | Binary: a source table built in `node`; adapters probe for hardware | Adapters feed measurements | The estimator | `clock` (adapters), estimator crate (X11) |
| Mesh clock state | Memory per node; published as `<node>.clock.offset` and `.clock.error` | `clock`; `node` publishes | `hub.now()`, `home` (fence, stamp limits) | `clock` |
| Operation table | Binary | The build | CLI, MCP, embedded docs | `ops` |
| Node key material | Node-local disk | `node` at join | `transport`, `node` | `node` |
| Per-node settings (disk budget, pool budget, data directory) | Open (5.1) | | `buffer`, `block` | Open |
| SDK guide, JSON schemas for editors | Generated from kinds and the operation table | `ops`, `init` | Agents, editors | `ops` |

---

## 3. Contradictions and resolutions

Each item names the conflict, the resolution, and its basis. Every resolution is
decided (2026-10-04). Most follow from a later locked entry; the rest fall under a
delegation.

### 3.1 The checks the user asked for

**X1. Node in the spec vs node membership as runtime state.**
Conflict: S8 defines `Node { key, name, public_key }`, and BQ2 lists `spec::Node`.
BQ11a makes joining an operation and membership runtime state. r3's example layout has
a `nodes.fdn` file.
Resolution: delete `spec::Node`. A node is a membership record in the region state of
the region that holds its name (`mesh`). The spec refers to nodes by name only (region
voters, placement, connector `node`, time policy). `plan` checks those names against
membership and warns, but does not fail, on a node that has not joined yet. Basis:
BQ11a (later, user-locked).

**X2. K5 voters policy vs the `region` block.**
Conflict: K5 locks `[[voters]] select = "site_a.**"`, the MODEL MAP says "Voters ->
regions via selector", r8 lists `Policy::Voters`, and C6 and r4 assume a voters
policy. REGION BLOCK replaces it with `region "<prefix>" { voters }`.
Resolution: a region is a definition, not a policy. Remove `Voters` from the policy
kinds and from `spec::resolve`. Governance is "longest region prefix that contains the
name". A root `region ""` block (or an implied root) holds the root voters. Basis:
REGION BLOCK (later).

**X3. One `voters` list in the region block vs two owners of the voter set.**
Conflict: the region block shows one `voters` list. K5 REVISION says the region changes
its own voters and the parent only creates, removes, or forces takeover. r4 stores
initial voters and the epoch in the parent.
Resolution: creating or removing a region block is a parent commit that writes
`{ prefix, epoch, initial voters }`. A later change to `voters` is the region's own
commit (joint consensus). The current voter set lives only in the region's Raft
config. `plan` sorts each region-block change to the right region. A forced takeover
bumps the epoch in the parent. Basis: K5 REVISION.

**X4. Per-kind schema validation in `config` vs kinds owning their config.**
Conflict: C3 ("typed config with schema"), the AGENT REQUIREMENT ("JSON Schema for the
config language"), BQ2 ("checks it against its schema"), and r8 section 1.14
(`config::load(files, kinds: &Schemas)`) put a schema check in `config`. KINDS OWN
THEIR CONFIG says `config` never knows a kind's fields.
Resolution: each kind parses and checks its own Document and returns diagnostics with
positions plus the channels it reads, writes, and defines. A schema is only an output,
generated from the kind's own types, for docs, editors, and agent guides. `config`
never runs a generic schema validator. Basis: KINDS OWN THEIR CONFIG (later).

**X5. C3 groups vs connector = task.**
Conflict: C3 REFINEMENT, r8 traces (k) and (l), and the r12 catalog use groups:
`compose::groups`, `run::Commands` per group, `ctx.writer(&InGroup)`, and a `Device`
error handler at group scope. GROUPS DROPPED removes groups.
Resolution: one connector is one task with one primary session. Remove
`compose::groups`. `ctx.writer()` and `ctx.reader()` take no group argument. The
`Device` error class is handled by the composition for the whole connector (stop, back
off, reopen). A re-index moves a channel between connectors, never between groups of
one connector (r8 trace (k) "variant" path). The r12 note "choose the shard by
connector" now has no ambiguity, because a connector writes one index. Basis: GROUPS
DROPPED.

**X6. Per-group run channels vs commandable parameters.**
Conflict: r8 Q14 proposes a run command channel and ack per group plus a `stopped`
boolean in the spec. r12 has `run::Commands` per group. GROUPS DROPPED gives every kind
a `running` commandable parameter.
Resolution: `running` is the only run state. It is a commandable parameter channel with
an ack under the connector's name, gated by access and control authority. The files
give its starting value; after that, the latest command decides, so arming never
drifts from the files. Rename the r12 component from `run` to `params`. Basis: GROUPS
DROPPED.

**X7. Calculation outputs: first input's index vs own index; one output vs several.**
Conflict: r3 puts the output on the first input's index (a second writer on that
index). C5 locks "outputs on the calc's own index". The MODEL MAP says "Calculation ->
one output channel"; the C5 lock says "outputs".
Resolution: a calculation writes one index of its own, with one or more output
channels on it. Update the model map edge to "Calculation -> inputs, -> its own output
index". Trade: an output stores its timestamps again, so raw counts plus calculated
scaling meets P1's byte target only for the wire, not for the receiver's disk. Basis:
C5 + KINDS OWN (later).

**X8. S1 and S2 vs M1 and M3.**
Conflict: S1 defines `Frame { keys: Vec<channel::Key>, series: Vec<Series> }`. S2
defines `Series { seq, data: Buffer }` and "compression known per channel". M1 and M3
replace both. M3 also keeps a `seq` in each series descriptor, but A8 counts seq per
index.
Resolution: M1 and M3 define the frame in memory. The S1 principle still governs the
wire. `Buffer` is now `block::Block`. Under the memory delegation, move `seq`
from the series descriptor to the index-group entry in the frame header, next to the
sample count, because all series on one index in a frame share it; descriptors become
`{ offset, len }`. Name the three numbering spaces: `channel::Key` (identity),
`channel::Slot` (node-local), and per-connection short numbers (`wire`). Basis: M1, M3,
R10, MEMORY/PERF DELEGATED.

**X9. References inside one region vs free placement.**
Conflict: r4 Q5 requires every key reference, including `home` and `standby`, to stay
inside one region. BQ8 and READ COPIES let homes and copies sit elsewhere. "r4
reconciliation" calls placement "free", but BQ8 still binds the standby.
Resolution: definition references (index, quality, error, control) must stay inside
one region; `plan` fails otherwise. Placement names nodes, not keys: the home node may
be in any region; the standby must be in the home node's region (BQ8); copies may be in
any region. Basis: r4 reconciliation, BQ8, READ COPIES.

**X10. S13 quality on its own index vs BQ13.**
Conflict: S13 says a quality channel has its own index and is written only on change.
BQ13 allows the data's own index and per-sample writes. r8 Q13 found that an ack and
its quality on two indexes can arrive out of order.
Resolution: BQ13 holds. "Written on change" applies to a quality channel on its own
index. A quality channel on the data's index appears in the frames where its writer
includes it (A7 subset rule). Under the quality delegation, an ack's quality
channel shares the ack's index, so an ack and its failure code arrive in one frame.
Basis: BQ13, quality delegation.

**X11. The `time` crate vs `clock`.**
Conflict: C1, BQ1, HOME SPLIT, C9a, r8, and r12 say `time`. R9-D13 renamed the layer-2
crate to `clock` and put `Stamp`, `Span`, and `Range` in `types::time`. The r12
catalog also has a connector module named `clock` (`clock::Software`,
`clock::Window`, `clock::Fit`). TIME ADAPTERS says the same estimator also serves
device clocks in the connector library, but layer 3 cannot use a layer-2 crate other
than `hub`.
Resolution: the layer-2 crate is `clock` (it drives sources and the peer exchange, and
serves `now()`). The pure estimator, `Measurement`, the exchange state machine, and
the oscillator fit move to a layer-1 crate (`estimate`), used by both
`clock` and the connector library. Rename the connector module to `stamp`
(`stamp::Midpoint`, `stamp::Window`, `stamp::Fit`). Basis: R9-D13, TIME ADAPTERS, the
SRP PASS layer-1 rule, BQ21 (names).

**X12. The old term vs "region".**
Conflict: S9, K5, the MODEL MAP, C3, C6, C8, BQ8, R4 RESULTS, r4, r8 (including its
tree method for one prefix), and r13 use the old word for the governed part of the
tree.
Resolution: read every such use as "region". The tree method becomes
`spec::Tree::region`. Keep the old word only for Git and for the possible future mesh
branching feature. Basis: REGION LOCKED.

**X13. A standby as a hub reader vs the `replica` component; and what `replica` may
call.**
Conflict: r8 Q6 and its simplification list make the standby a complete reader through
`hub`. BQ6 makes `replica` a layer-2 component "using only delivery raw subscription,
buffer append_at, transport". But `delivery` is a pure state machine whose per-index
instances live inside `home`, so `replica` cannot reach the send side without calling
`home`.
Resolution: the send side is a raw cursor in the home's `delivery` state (one more
holding cursor), served over the network by `hub`'s server loop, which already serves
every incoming session (BQ1). `replica` is the receive side only: it dials the home,
receives log records, and stores them with `buffer::append_at`. It never touches the
home's write path. Dependencies: `transport`, `wire`, `buffer`, `mesh`. Basis: BQ6,
BQ1, r13 section 5.3 ("the standby pulls").

**X14. Reader positions: a channel vs log records.**
Conflict: S9 says "reader positions at the home"; S10 says positions are kept per
reader and index and "current readers and their holds are status channels"; r8 Q6 puts
positions on a channel. BQ6 says positions travel in the log.
Resolution: one truth with copies. `delivery` owns positions at the home; it writes them
as index log records, so `replica` copies them in order with the data. A connected
reader's `hub` presents its own position when it resumes at a new home. Status channels
show positions for visibility only. Basis: BQ6, BQ11b.

**X15. Status written by a connector kind vs by `node`.**
Conflict: r8 Q11 adds a `connector-status` kind, and r13 section 6.1 repeats it. BQ11b
and r12 I1 put the collector in `node`.
Resolution: no `connector-status` crate. `node`'s collector pulls each layer-2 crate's
values and writes `<node>.*` channels through a `hub` writer session. A connector writes
its own status through `ctx.status()`. The home writes only its companion samples
(control channel, death records). Basis: BQ11b (later, user-locked).

**X16. One language for definitions and calculations vs calculation strings with their
own grammar.**
Conflict: r3 Q4 says one language because HCL has expressions. K1 replaces that with a
small calculation grammar written as a string and "checked at plan", next to "config
checks only the Document". C5 + KINDS OWN then make the calculation engine a separate,
powerful, later design owned by the calc kind.
Resolution: the calc kind owns its language, its parser, and its plan-time check.
`config` never parses calculation text. Files carry the program as a string attribute
in every syntax (K1). Whether a large program may live in its own file is open (5.1).
r3's single-expression and no-loop limits are gone. Basis: C5 + KINDS OWN (latest).

**X17. The crate list and layers after the SRP splits, `replica`, `ops`, `clock`, and
the layer-1 rule.**
Conflict: C1 and C9a list `types` and `codec` in layer 1 and `cli` and `mcp` as
crates. r8 adds `env`, `blob`, `ops`, `connector-status`. HOME SPLIT puts `control` and
`delivery` in layer 2 as leaves. SRP PASS adds `access`, `raft`, `wire`, and `block`
and defines layer 1 as "pure logic, no I/O" and layer 2 as "drives disk, network,
clock", while keeping the BQ1 order. R9-D13 keeps a `block` module in `types` although
SRP PASS made `block` a crate. r12 already calls `control` and `access` layer 1.
Resolution: every pure crate is in layer 1: `block`, `types`, `env`, `document`,
`raft`, `estimate`, `control`, `codec`, `wire`, `spec`, `access`, `delivery`. Layer 2 is
`transport`, `buffer`, `clock`, `blob`, `mesh`, `home`, `replica`, `hub`, and `sim`. No
`cli`, `mcp`, or `connector-status` crates. `env` (from r8, never locked) is adopted
because T1 needs one home for the injected seams. Later additions under the
architecture delegation: `ring` (layer 1) splits the cross-shard rings and wake
protocol from `block`, and `os` (layer 2) holds the real implementations of the `env`
seams, so no other crate touches the OS directly. Full map in section 4. Basis: SRP
PASS, BQ6, BQ17, BQ19, BQ11b, R9-D13.

### 3.2 Data structures defined in two places

**X18. Control state after failover: read from the channel vs internal records.**
Conflict: r13 section 6.2 starts the new gate from "the copied control channel". BQ11b
forbids core decisions that read channels back. BQ6 lists "control handoffs" among the
log records.
Resolution: the new home's `control` takes its starting state (last holder, "held, not
connected" for one lease period) from the handoff records in the index's log. The
control channel is a published copy that the home writes. The control
channel sits on its own small index placed with the controlled index, because
home-written samples on the controlled index would break S6 strict increase against
the writer's device timestamps. Basis: BQ6, BQ11b, failover delegation.

**X19. Death records vs one writer per index.**
Conflict: QUALITY DECISIONS has the home write "source lost" to "each affected index's
quality channel(s)". A quality channel can be shared by many channels and sit on an
index homed elsewhere and written by another session (S13). The home would then write
another node's index. A18's "Foundation never judges quality" also reads against it.
Resolution (quality delegation): when a writer session ends without closing, each home
writes "source lost" only to the quality channels that the lost session itself was
writing, on indexes that home homes. The sample is stamped with mesh time. Later data
from the writer that is older than that sample must arrive as backfill (A6). This is a
recorded fact, not a judgment of values. Basis: quality delegation.

**X20. "One writer per index" used two ways.**
Conflict: A1 allows many writer sessions with one in control. A7 says "one writer in
control". r12 enforces "one writer per index" at `open_writer`. BQ15 says "never two
writers".
Resolution: two rules with two names. Runtime: at most one writer in control per index
at a time (the gate). Plan: at most one connector writes an index. The home itself may
add companion samples (control channel, death records) on indexes it homes. Basis: A1,
S11, BQ15.

**X21. The Document type and shared parsing parts have no layer-1 home.**
Conflict: K1 puts Document checks in `config` (layer 4). BQ2 stores connector config as
an opaque document in `spec` (layer 1). KINDS OWN THEIR CONFIG gives kinds (layer 3) a
shared Document reader with positions, name and unit parsing, and diagnostics. KINDS
OWN also says "`config` parses files to Documents", but K1 says front ends parse.
Resolution: a layer-1 crate `document` holds the Document, source positions,
diagnostics, and readers for durations and rates. Unit names live in `spec::unit`, name
syntax in `types::name`. Front ends (`config-hcl`) parse files; `config` reads only
Documents. Basis: K1, BQ2, KINDS OWN, "decide the best architecture".

**X22. Where a connector runs: the connector's `node` vs placement.**
Conflict: C3 and C5 SHAPE give each connector a `node` attribute. BQ10 says a placement
covers a connector and every index under its name. B7 gives indexes a default home on
the connector's node. A placement selecting the same connector could name another node.
Resolution: the connector's `node` is its required primary node (it is
attached to a device, and `discover` writes it). A placement that selects a connector
may add `standby` and `copies` but may not move its primary; `plan` fails if it tries.
An index's home, in order: a placement that selects the index, then the node of the
connector that writes it (B7), then a plan error. Basis: BQ10, B7, C5 SHAPE.

**X23. The `index` edge stated twice.**
Conflict: S5 puts `index` on the data channel, and BQ9 re-indexes "by changing `index`
in the file". C3 REFINEMENT has the connector name its index and channels.
Resolution: the data channel definition owns the `index` edge. A connector's config
names the index it writes; its checker confirms that every channel it writes points at
that index. A struct template instance names one index for all its fields. Basis: S5,
BQ9, KINDS OWN.

**X24. Placement of companion channels.**
Conflict: Q11 (r8) requires the control channel on the same home as its index; r13 Q10
puts "command indexes and their control channels" under one placement; BQ13 lets a
quality channel share the data index. No entry says which index the ack, parameter,
error, and status channels use.
Resolution: an out connector writes acks and its status on one small index
of its own; parameter commands sit on one per-connector parameter index (one control
gate for all parameters, homed on the connector's node so arming works while cut off,
as r8 Q14 advised); the error channel is written by the connector that writes the
index. `plan` fails if a companion would be homed away from the index it serves. Basis:
BQ10, r8 Q14, BQ13.

**X25. Access combination vs the one resolver.**
Conflict: BQ2 makes `spec::resolve` "the ONE policy resolver" with most-specific-wins,
and S12 lists access as one of those policies. C8 makes access allow-only with no
conflicts (a union of allows).
Resolution: `spec::resolve` applies most-specific-wins to setting policies (retention,
placement, transmission, compression, reduction, time, secret store). Access is
evaluated only in `access`, as the union of matching allows; the authority cap is the
highest authority among matching allows that grant `write`. Both use the one selector
matcher in `types`. Basis: C8, SRP PASS (`access` split).

**X26. Policy targets and reach.**
Conflict: S12 says "policies apply to whole indexes; data channels follow". Reduction
selects data channels (it is checked against a channel's unit). Access selects any
name and subjects. Time selects node names. Secret store selects secret names. BQ10
makes placement select connectors. r3 K2 forbids a policy from selecting outside its
region; r4 lets a root policy apply inside child regions.
Resolution: each policy kind states its target: retention, transmission, and
compression select indexes; placement selects connectors and indexes; reduction selects
data channels; time selects nodes; access selects names (plus subjects anywhere);
secret store selects secret names. A policy may select only names in its own region
and that region's descendants; a descendant applies it as of the last parent version
it saw. Basis: S12, REDUCTION, C8, C6, r4 Q5.

**X27. Built-in channels have no spec definitions.**
Conflict: S8 puts node status under the node's name, and S9 adds the changes channel.
Nodes are runtime membership (BQ11a). Keys come from `apply` (M1/M2 answer), and only
`apply` changes the spec.
Resolution: built-in channels are defined by the binary, not by files. A
node's status channels are a fixed set per release, recorded with the membership record
at join, with keys assigned then. A region's changes channel is created with the
region. `hub` resolves names under a node name through membership. `plan` shows these
channels as read-only. Basis: BQ11a, BQ11b.

**X28. Runtime channel creation (A2) vs "only apply changes the spec" and "anything that
creates a channel is an explicit definition".**
Conflict: A2 lets the first write create a channel in an open folder, committed by
voters. K3 says only `apply` changes the spec. The REDUCTION rule requires an explicit
definition for every new channel.
Resolution: the open folder is itself an explicit definition in files
(not a policy). Inside it, a first write is a create operation (access action `write`
on the folder) that the folder's region voters commit to the spec and log on the
changes channel. `plan` lists such channels until files adopt or delete them. K3 then
reads: the spec changes only through `apply` and open-folder creation, and both are
logged. Basis: A2, K3, REDUCTION.

**X29. The changes channel: one for the mesh vs one log per region; its name.**
Conflict: S9 names one `mesh.changes` channel with seq equal to the Raft log index. K5
and R4 give each region its own Raft log. The name `mesh.changes` also does not use the
reserved `@` prefix (A3) and collides with user names that start with `mesh`.
Resolution (names delegation): one changes channel per region, named with the reserved
segment, for example `site_a.@changes` (root: `@changes`). `mesh` serves it; `hub`
routes subscriptions to `mesh`. Basis: K5, R4, A3, BQ21.

**X30. Catch-up merging vs presence per frame.**
Conflict: B6 says catch-up merges consecutive frames because "frame boundaries carry no
meaning". QUALITY DECISIONS makes optional-field presence per frame.
Resolution (memory delegation): catch-up merges only consecutive frames with the same
key set and presence mask. On disk, presence comes from the seq runs each chunk
records. Struct views use presence per sample range. Basis: QUALITY DECISIONS.

**X31. Timestamp ties: A5 vs S6.**
Conflict: A5 allows equal timestamps ordered by seq. S6, r8, and BQ4 require strict
increase per path.
Resolution: strict increase per index per path. A5's tie rule is retired, and the
InfluxDB tie concern goes with it. Basis: S6 (later).

**X32. The max-age check at the home vs at the reader.**
Conflict: B4 has the home skip a newest frame older than `max_age` for a new reader.
r8 trace (c) checks only at the reader's `hub` ("no second guard").
Resolution: one check, at the reader's `hub`, against the mesh time interval. Basis:
root "no defense in depth".

**X33. Where channels come from.**
Conflict: S5 and A2 say files list every channel. r3 lets a calculation infer and
define its output channel. GROUPS DROPPED creates `running` and parameter channels for
every connector. S7 creates field channels from a struct instance. C3 puts status
under each connector.
Resolution: a channel exists only when a definition in files creates it,
directly (a channel or index block) or by implication (a struct instance's fields; a
connector's status, parameter, and ack channels; a calculation's output index and
channels). A kind reports the channels its definition implies at check time. `plan`
lists every channel with the definition that made it, and `explain` shows it. This
keeps the REDUCTION rule, because a policy never implies a channel. Basis: REDUCTION,
KINDS OWN, S7.

**X34. Compression on links vs per-vector tags.**
Conflict: S2 says "compression known per channel" and "re-encode only for links with
different compression". B6 lists "compression level" as a per-link transmission
setting. R10 and BQ4 use a tag in every vector, encode once, and a `compression` policy
per index.
Resolution: no per-link re-encode and no compression field in transmission. The
compression policy decides at the encoder. Whether `max` belongs on thin links is a
measurement question (R10-D7). Basis: BQ4 (later).

**X35. "One byte format" for memory, wire, and disk.**
Conflict: S2 wants one byte format. r2 splits it: plain layout in memory, compressed
bytes on disk and in catch-up, stateful framing on the live wire. BQ4 encodes once at
entry.
Resolution: encoded series bytes (tagged vectors) are produced once and shared by disk,
catch-up, and live wire. Only framing (key set numbers, predicted seq and counts) is
per-connection state in `wire`. Memory holds raw bytes for local writes until the home
encodes. Basis: BQ4, R10-D3.

**X36. A fixed time source vs following the smallest bound.**
Conflict: C6 shows `[[time]] select = ... source = "site_a.gps_1"`. R6 TIME LOCKED
follows the smallest measured bound with no fixed ranking and detects sources.
Resolution: the time policy lists only the peer nodes a node may use as mesh
references (default: its region's voters). Local hardware sources are found
automatically. The estimator always follows the smallest bound. Basis: R6 TIME LOCKED,
TIME ADAPTERS.

**X37. Bootstrap peers and relays.**
Conflict: D7 puts bootstrap peers in the file and lets any public node relay. R5 drops
"every node can relay" and puts relays in designated nodes "chosen by policy". BQ11a
tickets carry voter addresses.
Resolution: no peer list in files; join tickets carry the first addresses, and after
that addresses come from the mesh. Relays are designated by a
`relay` policy that selects node names, like the time policy. Basis: BQ11a, TRANSPORT
SHAPE.

**X38. The spec tree shape and gossip.**
Conflict: S9 describes a Git-like tree that follows the name hierarchy and gossip for
hints. R4 SETTLED uses one prolly tree per region keyed by full name and no gossip.
Resolution: R4 SETTLED. Basis: later entry.

**X39. The region of the index history.**
Conflict: r8 Q9 keeps the history in "the group that governs the channel's name". BQ9
says the old home records the seal with "its region's voters", and BQ8 keeps an index's
runtime records in the home node's region. The two differ when a cloud node homes a
site's index.
Resolution: keep the whole history in the channel name's region,
so a reader on any node finds it by name, and so the region that commits the re-index
also holds it. The old home proposes its seal there. If the old home is unreachable,
that region seals at the old home's lease end, which it reads from the home node's
region. Basis: r8 Q9, BQ9.

**X40. Secret names and which region holds ciphertexts.**
Conflict: K4 uses flat names (`secret = "influx_token"`). C8 and the SIMPLICITY
DIRECTIVE put everything in one name tree, governed by regions. BQ16 says "voters store
ciphertexts" without saying which region.
Resolution: secrets are full names in the tree (for example
`site_a.secrets.influx_token`). The region that holds the name stores its ciphertexts.
Access (`secret` action) and the secret store policy select secrets by name. Basis: C8,
SIMPLICITY DIRECTIVE.

**X41. Writer-side buffering.**
Conflict: A1 and A20 say "writes to the home are never buffered". BQ7 and r13 let a
writer keep unconfirmed frames and resend them after failover.
Resolution: live delivery is never queued for retry. A writer may keep unconfirmed
frames in a bounded memory window only to resend them as backfill after failover (B7).
Basis: BQ7, B7.

**X42. The owner of the slot and key set tables.**
Conflict: M1 needs one node-wide slot table and key set interner. `hub` opens sessions,
but `home` (below `hub`) routes by key set and writes companion samples, so a
`hub`-owned table would point upward.
Resolution (memory delegation): the interner is a layer-1 data structure
(`types::frame::Interner`). `node` constructs one instance per node and injects it into
`hub` and `home`. Interning happens at session open; each shard reads a snapshot.
Basis: M1, root principle on injected registries.

**X43. Which crate serves readers at a read copy.**
Conflict: READ COPIES has remote readers read and hold at the copy, but no entry says
which crate serves them. `replica` only receives.
Resolution (failover delegation): `home` opens a copied index in copy mode (the
crash-recovery open without a write path, gate, or seq) and serves readers through its
`delivery`. Holds at the copy are local to the copy. Basis: BQ6, READ COPIES.

### 3.3 Layer rules

**X44. C1 rule 1 ("layer 3 uses only `hub`") vs what connectors need.**
Conflict: connectors also need `types`, `block`, `spec`, `document`, the estimator,
and `env` seams.
Resolution: layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
`ctx` hands out the `env` seams. Basis: C1, BQ3, BQ5.

**X45. C1 rule 2 ("only layer 2 does I/O") vs connectors, front ends, `node`, and
secret adapters.**
Conflict: connectors open sockets and call vendor libraries; front ends and `node`
read files; secret adapters call Vault and cloud services.
Resolution: layer 1 does no I/O. Layer 2 does mesh I/O (peers, the disk buffer, the
clock). Layer 3 does outside-system I/O only through injected dialers, links, and
dedicated vendor threads. Layer 4 does process, file, and OS I/O. All of it enters
through injected seams, so simulation can replace it. Basis: T1, SRP PASS.

**X46. Where access is enforced.**
Conflict: BQ5's lock text says "hub and home enforce the rules (... access)". r8 Q12
and r12 enforce access only at the owner. BQ12 found that "authenticate at `hub`,
authorize at the owner" lets a forwarding node impersonate a subject.
Resolution: enforcement only at owners (`home` for data; region voters for apply,
secret, admin), with no check in `hub` or `ctx`. The owner learns the true subject
from the signatures it verifies (BQ12). Basis: root "no defense in depth", r12 table.

### 3.4 Terms used two ways

**X47. "Lease".** It means a node lease (failover), a control lease (writer setting),
and r12's `endpoint::Lease<T>`. Resolution: in prose, always "node lease" or "control
lease". Rename `endpoint::Lease<T>` to `endpoint::Handle<T>`. (Names delegation.)

**X48. "Slot".** It means `channel::Slot`, the latest-value slot (r8, B4), a kind
table slot (C5 SHAPE), and a memtable slot (r2). Resolution: "slot" means only
`channel::Slot`. Use "latest mailbox", "table entry", and "memtable entry". (Names
delegation.)

**X49. "Kind".** It means the channel enum (`Kind::Index`, `Kind::Data`), connector
kinds (`kind = "opcua"`), and policy kinds. Resolution: user-facing "kind" means a
connector kind only. Prose says "index channel" and "data channel", never "channel
kind". The internal `spec::channel::Kind` stays namespaced. (Names delegation.)

**X50. "Block".** It means a Document block (HCL) and a pool buffer (`block::Block`).
Resolution: keep both, namespaced (`document::Block`, `block::Block`). Prose says
"config block" and "pool block". (Names delegation; flagged in the log, BQ21.)

**X51. "Integration", "sink", "durable reader", "mesh file".** These retired terms still
appear in A11, A16, A19, B1, S10, K4, C8, and the reports. Resolution: read
"integration" and "sink" as "out connector", "durable reader" as "named reader with a
hold", and "mesh file" as "definition files".

**X52. Decision IDs.** r9 numbers its decisions D1 to D14, which collide with the log's
D1 to D7; r10 uses D1 to D8; r12 and r13 number theirs too. Resolution: cite them as
R9-Dn, R10-Dn, R12-n, and R13-n (as this record does).

**X53. D6 path lock vs C9c.** D6 says agents "cannot edit" contract and oracle paths.
T2 calls that too strict, and C9c enforces oracles by visibility only. Resolution: C9c.
People still own contracts and oracles; agents may edit them, and every weakening gets
an adversarial reviewer and a person's merge.

**X54. "Clock".** It means `env::clock::Clock`, the monotonic clock of one node, and
the `clock` crate, which serves mesh time. Resolution: in prose, "monotonic clock" for
the `env` seam and "mesh clock" for what the `clock` crate serves.

Count: 54 items (X1 to X54).

---

## 4. Final crate map

Rules:

1. A crate depends only on crates earlier in its own layer or on lower layers.
2. Layer 1 is pure: no I/O, no clock reads, no threads, no globals. It decides.
3. Layer 2 drives the mesh's own disk, peer network, and clock. It does.
4. Layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
5. Layer 4 may depend on anything below it.
6. Upward flow goes only through values the upper crate pulls (watches, streams).
   Seams that lower crates define and upper crates implement (`env` traits) are
   injected downward.
7. Only `os` touches the real clock, files, network, randomness, and threads, through
   the `env` seams it implements. Only `clock` reads wall time through `env`; everyone
   else asks `clock`. Only `node` builds real seams, and only `sim` builds simulated
   ones. Below `hub`, only `home` writes channels, and only its companion samples.

Order: layer 1 (`block`, `ring`) -> `types` -> (`env`, `document`, `raft`, `estimate`,
`control`, `delivery`) -> `codec` -> `wire` -> `spec` -> `access`; layer 2 `os` ->
(`transport`, `buffer`) -> (`clock`, `blob`, `sim`) -> `mesh` -> (`home`, `replica`) ->
`hub`; layer 3 `secret`
-> `connector` -> `connector-<kind>`; layer 4 (`config-hcl`, `config`) -> `ops` ->
`node`.

| Layer | Crate | Job (one sentence) | Allowed dependencies |
| --- | --- | --- | --- |
| 1 | `block` | Owns pools of preallocated, aligned buffers (`Pool`, `Unique`, `Block`, one refcount per frame, offsets only) and their unsafe memory code. | none |
| 1 | `ring` | Carries handles between shards through bounded single-producer, single-consumer rings, owns the wake protocol (loom-checked), and holds its own unsafe slot code (memory delegation, 2026-10-04). | none |
| 1 | `types` | Defines byte-level values: time, sample types, series, frames, key sets, views, keys, slots, quality, names, and the one selector matcher. | `block` |
| 1 | `env` | Defines the injected seams for monotonic time, the OS wall clock (read only by `clock`), files, randomness, threads, and task spawning. | `types`, `block` |
| 1 | `document` | Defines the syntax-neutral Document with source positions, diagnostics, and shared value readers. | `types` |
| 1 | `raft` | Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing about specs. | `types` |
| 1 | `estimate` | Computes clock offset and error bounds from measurements, the peer exchange, and device oscillator fits. | `types` |
| 1 | `control` | Decides who holds control of an index: authority, ties, control leases, handoffs, start state after failover. | `types` |
| 1 | `delivery` | Keeps each reader's state per index: cursors, credits, latest mailbox, holds, floors, position records, masks. | `types`, `block` |
| 1 | `codec` | Compresses and checks one series: per-vector selection, codecs, header validation, format version. | `types`, `block` |
| 1 | `wire` | Defines every message between two nodes: per-connection short numbers, predicted seq and counts, session, credit, and replication messages, format version. | `types`, `block`, `codec` |
| 1 | `spec` | Defines the definitions (channels, types, units, connectors with opaque config, regions, policies, open folders), the prolly tree, hashes, diffs, and `spec::resolve`. | `types`, `document` |
| 1 | `access` | Decides whether a subject may do an action on a name: union of allows, authority cap. | `types`, `spec` |
| 2 | `os` | Implements the `env` seams and `block::Memory` on the real operating system: monotonic and wall clocks, files, sockets, memory, randomness, and threads. The only crate allowed to call them. | `env`, `types`, `block` |
| 2 | `transport` | Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS over TCP, relays, and diodes on the `env::net` seam; never calls up. | `env`, `types`, `block` |
| 2 | `buffer` | Stores each index's log durably within the disk budget (write-ahead ring, segments, trimming, floors, `append`, `append_at`) through a per-OS driver. | `env`, `types`, `block`, `codec` |
| 2 | `clock` | Runs time source adapters and the peer exchange, feeds `estimate`, and serves mesh time as an interval. | `env`, `types`, `estimate`, `wire`, `transport` |
| 2 | `blob` | Stores content by hash and fetches it from peers (spec chunks, binaries). | `env`, `types`, `block`, `wire`, `transport` |
| 2 | `sim` | Simulates the `env` seams (time, randomness, scheduling, files, network) with a deterministic scheduler and fault injection; ships behind a feature. | `env`, `types`, `block` |
| 2 | `mesh` | Agrees per region, through `raft`, on spec pointers, delegations, and runtime state (membership, node leases, homes, seq blocks, index history, secret ciphertexts, tickets, versions, rollout lock, format flag); serves snapshots, watches, effective settings, and the changes channels. | `env`, `types`, `raft`, `spec`, `access`, `wire`, `transport`, `clock`, `blob` |
| 2 | `home` | Runs the per-index write path (time checks, seq, fence, control, storage, fan-out), crash-recovery and copy-mode opens, and companion writes. | `env`, `types`, `block`, `ring`, `control`, `delivery`, `codec`, `spec`, `access`, `buffer`, `clock`, `mesh` |
| 2 | `replica` | Receives an index's log from its home on a standby or copy node and stores it with `append_at`. | `env`, `types`, `block`, `wire`, `transport`, `buffer`, `mesh` |
| 2 | `hub` | Is the one path for every read and write: sessions across homes, routing, live selectors, the server loop, authentication, encode and decode once, raw cursors for replicas, re-index stitching, and the layer-3 window. | `env`, `types`, `block`, `ring`, `codec`, `wire`, `spec`, `transport`, `clock`, `mesh`, `home` |
| 3 | `secret` | Resolves a named secret on the node that runs a connector, through store adapters chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`. | layer 1 |
| 3 | `connector` | Defines the kind contract (parse, check, discover, run), the thin supervisor, `ctx`, the component library, and the compositions. | layer 1, `hub`, `secret` |
| 3 | `connector-<kind>` | Translates one protocol, device family, store, or the calculation engine into channels. | layer 1, `hub`, `connector`; vendor libraries behind build flags |
| 4 | `config-hcl` | Reads and writes HCL files as Documents and keeps formatting. | `document` |
| 4 | `config` | Checks core definitions in Documents, expands templates, hands connector blocks to kinds, and computes plans, explains, and exports. | layer 1, `connector` |
| 4 | `ops` | Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and runs each operation on the node that must run it. | `config`, `connector`, `hub`, `mesh`, `blob`, `sim`, layer 1 |
| 4 | `node` | Is the composition root: real seams, pools and shards, all tables (kinds, front ends, time sources, secret stores), the status collector, process lifecycle, and upgrades. | all crates |

Outside the binary: the Rust SDK reuses `block`, `types`, `codec`, and `wire`; other
SDKs hand-write their data path against golden vectors (D12).

---

## 5. Open items

### 5.1 Still open

Shapes that need the person:

1. **Calculation engine design.** Language, windows, state, placement, quality
   propagation, and whether a large program lives in its own file. Its guarantees are
   locked (C5 + KINDS OWN).

Measured before they lock:

2. **C2, thread model.** The working assumption is in 1.5. The `memory` builder
   measures the handoff on Linux first (#9).
3. **OPC UA crypto plugin.** Our own plugin on aws-lc, or compiled-in mbedTLS.

Parameters and later choices, recorded and not asked:

4. Struct template storage: whether the spec stores templates and instance records for
   SDK code generation and `export`.
5. Per-node settings (disk budget, pool budget, data directory): node-local config or a
   policy that selects node names.
6. The transmission policy target: links, indexes, or both (B6).
7. Upgrades across regions: which region holds the desired version and the format
   flag, and how finalization waits for every region (BQ18, C9d).
8. R12-4: a spec change restarts `run` in v1; commandable parameters are the runtime
   path.
9. A20: whether a channel may carry a default max age.
10. A3: partial-segment wildcards.
11. A13: bounded lists.
12. D3: license, free tier, monetization.
13. D5: a plugin system.

### 5.2 Settled under a delegation

- Quality: X10 (ack quality on the ack's index), X19 (death record scope), R16-1 and
  R16-3 to R16-9 (r16 Rust guides).
- Memory and performance: X8 (seq per index group), X30 (merge rule), X42 (interner),
  S4 disk format starting point, r12 I4 (`buffer` driven, not self-running), `ring`
  holds its own unsafe slot code (section 4).
- Failover: X18 (gate start from log records, R13-5 "held, not connected" grace), X43
  (copy mode), R13-10 (three voters for failover; `plan` warns with fewer), R13-6 (send
  after sync vs on receipt).
- Names: X11 (`estimate`, `stamp`), X12, X29 (`@changes`), X47 to X50, X52.
- Architecture: X17 and section 4 (`env`, `document`, `estimate`, `secret` crates), X21,
  X44, X45; R12-3 error classes without groups; R12-7 vendor code only in dedicated,
  never-detached threads; R12-13 no always-on scan loop; R12-14 one cycle engine per
  connector.

### 5.3 Parameters for experiment

- Delivery and wire: group commit interval, credit window (bytes, from link BDP), batch
  size, linger, max packet size, latest-over-TCP send buffer, priority mapping, catch-up
  merge size.
- Storage: write-ahead ring size, ring record alignment, segment flush size and age,
  chunk sizes, memtable cost per channel (estimate 100 to 200 bytes), eviction timing.
- Codecs: ALP refresh interval and skip rule (R10-D8), natural-order delta decode speed
  on a Pi 4 (R10-D5), fdelta on recorded plant data (R10-D4), when to build `max`
  (R10-D7), short-vector packing.
- Memory: allocator (mimalloc 3 off the hot path), pool size classes and budget, commit
  and purge policy, queue kinds and capacities, spin windows (0 on a Pi), latest
  mailbox mechanics, cache-line padding.
- Replication: seq block size, node lease length, check-in period, fence margin, gate
  grace, standby send point (after sync or on receipt), SSD rule for Pi homes.
- Consensus and spec: Raft timeouts, prolly chunk size (~4 KiB) and chunker quality,
  root GC depth (last N roots).
- Time: exchange period, source discovery period, drift rate for bound widening
  (starts at 200 ppm, ESTIMATE COMBINE), stamp limits near 1970 and far future (A5).
- Transport: default carrier per traffic class (QUIC vs TLS over TCP, measured on
  Linux), GSO and GRO, ChaCha20 vs AES by platform, relay selection.
- Compression and reduction defaults; retention defaults; disk budget defaults.
- Benchmark reruns owed: r1 handoff, r10 codecs, r11 memory on Linux x86-64 (pinned)
  and Raspberry Pi 4; `sim` binary size against P1 (BQ19); binary size and idle memory
  of R7 dependencies.

### 5.4 Deferred features

- Cross-company sharing (K5).
- SSO mapping a login to a subject (C8).
- Consumer groups for named readers (S10).
- Configurable latest-mode depth (B4).
- Hot standby connectors (r13 Q10).
- Synchronous replication or per-placement durability (BQ7 says none for now).
- Shared-memory SDK transport (M5).
- Unions (A14), f16, 128-bit integers, decimals (A9).
- Own PTP client; Starlink dish, White Rabbit, and timing card sources (R6, TIME
  ADAPTERS).
- OS clock steering where privileged (opt-in, C6).
- More front ends (TOML and others); YAML write support (K1).
- A Zenoh connector (R14).
- A FIPS build (R7).
- A plugin system (D5).
- Copy-on-write mesh branching, the reason the word "branch" is reserved (VOCABULARY).

### 5.5 First phase

The first wave builds the riskiest pieces in parallel: `block` and `ring` (`memory`),
`types`, `codec`, and `wire` (`data-path`), `raft` and `spec` (`consensus`), and
`env`, `os`, and `sim` plus the QUIC against TLS over TCP benchmark on Linux
(`simulation`). The second wave adds `control`, `delivery`, `access`, then `buffer`,
`home`, and `replica` for a single-node write path measured against P1, then
`transport`, `mesh`, `clock`, and `hub`.
