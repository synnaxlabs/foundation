# Interview log

The chronological log of the design interview (2026-10-04), with the person's words.
It is history: where it and `docs/decisions.md` differ, `docs/decisions.md` wins. Read
it to learn why a decision was made.

Foundation is a new Synnax Labs product started 2026-10-04: one Rust binary, any OS or
cloud, a node in an industrial data and control mesh. Lives in the monorepo but shares
NO Synnax code, only lessons. Pillars: own sample-transport protocol over reliable and
unreliable links (networking, proxying, encryption setup, deployment, topology);
high-quality connectors (OPC UA, Modbus, MQTT, Kafka, NI, LabJack) with PTP/NTP setup;
high-quality outbound integrations (InfluxDB, Kafka, MQTT, Ignition, Grafana);
agent-operable; whole mesh as code. User positioning: headless, agent-driven,
developer-first, "not IT/OT ETL". Built by an agentic software factory.

Process: design skill + interview skill, one question per message, then an RFC in
`docs/tech/rfc/`. Research ledger (session scratchpad): foundation-research.md.

**Key research conclusions:**
- Per-hop reliability is not end-to-end; need origin sequence numbers, a durable
  source buffer, dedup on (origin, seq), fsync before ack.
- iroh 1.x gives dial-by-key QUIC, hole punching, and relays in pure Rust.
- NI DAQmx and LabJack need our own runtime-loaded (dlopen) bindings; OPC UA pure-Rust
  crypto has gaps; Kafka transactions only via librdkafka.
- Max plans: Consumer Terms forbid automated access except via API key; an unattended
  factory belongs on API keys or Team/Enterprise, not pooled Max logins (verified
  2026-10-04).

**Factory constraint:** two people, each on their own individual Max plan, sessions
run locally on their own machines. Design the factory around attended, locally
started sessions (workflows inside interactive sessions), not an unattended daemon.

**Interview mode (user, 2026-10-04):** "I want to walk through every single data
structure and key design decision one by one... I don't agree with any of these
things until we refine them together." Engineering decisions are NOT decided
unilaterally for Foundation; each is proposed with a concrete struct sketch and
locked only on agreement. Order: data model, delivery, wire protocol, mesh file and
security, connectors and integrations, calculation engine, time sync, agent
interfaces, factory.

**Locked decisions:**
- D1 (2026-10-04): Foundation is a mover of data, not a database. Nodes keep a
  durable buffer (hours to days) for store-and-forward; long-term storage and queries
  belong to the stores it pushes to. No historian, query language, or retention.
- D2 (2026-10-04): Foundation is bidirectional. Connectors carry commands (setpoints,
  valve writes) through the same device owner that reads. No control logic runs in
  Foundation. Commands expire at a deadline, are never replayed after an outage, and
  return an acknowledgment. First release includes command authority and audit.
- D3 (2026-10-04, revised same day): Monetization is deferred. "Build the best
  product possible and figure that out later." Product decisions must not be shaped
  by what we sell. The node is free. License (BSL was agreed before the revision) and
  free tier are open questions.
- D4 (2026-10-04): The exact connector and integration list is a parameter. The
  design requirement is that adding one is cheap: one small contract (typed config,
  one device owner, shared error set, read/write/command), a conformance kit plus a
  protocol simulator per connector, and one explicit compiled-in table (C-dependent
  ones behind build flags). First user: hardware test and ops teams; first acceptance
  scenario: remote test site (NI, LabJack, PLC over Starlink/cellular) streaming to
  cloud stores and laptops with no loss, synced clocks, commands back.
- D5 (2026-10-04): Developers extend Foundation through SDKs (Python, Rust first),
  not in-node plugins. User said "fine for now": a plugin system is an open question.
- D6 (2026-10-04): Humans own contracts and oracles; agents own code. The two humans
  write and review the RFC, the interfaces (connector contract, wire format, mesh file
  schema), and the oracles (scenarios, conformance kits, simulation invariants);
  agents cannot edit those paths. Agents write all other code; a fresh-context agent
  reviews each change; machine checks gate merges; humans sample-read. Foundation gets
  its own review rule in the monorepo.
- D7 (2026-10-04): The mesh is its own control plane. "We're shipping a single binary
  and a single binary only." No separate control-plane component or service. A few
  nodes named in the mesh file agree on changes with Raft (proven library, never
  homegrown); others keep a copy. Joining via `foundation join <ticket>` through any
  member. Bootstrap peers in the file; any public node relays. Status is streams. A site can be
  its own mesh, linked to others, so it can change local config while cut off.
- A0 (2026-10-04): The basic unit of data is called a **channel** (user's word; the
  first users' vocabulary via NI, LabJack, Sift, Synnax; "stream" collides with QUIC
  streams and Rust `Stream`; "tag" collides with InfluxDB tags).
- Vocabulary (user, 2026-10-04): identifiers are **keys**, never IDs. Apply the
  namespace rule: `node::Key`, `channel::Key`. Open item for security section: a
  node's key may be its iroh Ed25519 public key (one identity, not two).
- A1 (2026-10-04): One home node per channel: orders samples, keeps the durable
  buffer, runs the control gate. Standby home named in the mesh file takes over on
  failure; sequence numbers are (epoch, seq). Reads served from any copy. Many
  writers; one holds control at a time: highest authority (u8), tie keeps the first.
  Fixes over Synnax: control is a lease (quiet holder loses it), unknown control
  state fails closed, every handoff published on a channel, no shared mode. Writes
  from anywhere (events, notes, logs) use per-writer channels under a prefix, read by
  pattern subscription (e.g. `events/**`) merged by time. Sketch: `channel::Channel
  { key, name, data_type, home: node::Key }`, `writer::Writer { subject, authority,
  channels: Vec<channel::Key> }`.
- A2 (2026-10-04): The mesh file lists every channel. Hardware channels enter via
  `foundation discover <url>` writing into the file (review the diff, apply). The file
  can open a name folder (e.g. `lab/scratch/**`) to one program; inside it, the first
  write creates the channel (committed to mesh state via the voters). `foundation
  plan` lists runtime-created channels as not-in-file until added or deleted. Cost:
  runtime creation needs the voters reachable.
- A3 (2026-10-04): Names are dot-separated (NATS, OpenTelemetry, Kepware, PLC struct
  members), e.g. `site_a.stand_3.pt_101`. Hierarchy and struct fields share the dot:
  `site_a.motor_1.speed` reads the same whether `speed` is a channel or a field of
  struct channel `motor_1`; no channel may be created under a struct channel's name.
  Segments: letters, digits, `_`, `-`. Case-sensitive; case-only collisions rejected.
  Patterns `*` (one segment) and `**` (any depth); partial-segment wildcards are an
  open parameter. `@` prefix reserved for Foundation. MQTT integration maps `.`<->`/`.
- A4 (2026-10-04): `channel::Key` is a UUIDv7 (time-ordered), created with the
  channel, never reused, never changes; names can change. Users work with names;
  buffer, sequence numbers, and subscriptions follow the key. Renames are explicit:
  `foundation rename old new`, shown in `plan` as renamed (Terraform `moved`
  precedent). Rejected u32 (needs a shared counter; clashes across linked meshes;
  Synnax lesson) and u64 (JSON/JavaScript 53-bit precision loss; Synnax hit it). On
  the wire, each connection swaps keys for short numbers (detail for the wire
  section). Integrations decide rewrite vs alias for name-keyed external stores.
- A5 (2026-10-04): Timestamp = i64 nanoseconds since Unix epoch, UTC (not TAI; the
  time service converts PTP TAI at the edge; leap seconds dormant since 2016, ending
  by 2035). Per channel, timestamps are non-decreasing; the home rejects backwards
  samples; ties ordered by sequence number (no fake 1 ns steps). Home rejects
  near-1970 and far-future stamps (both limits are settings). InfluxDB integration
  must handle same-timestamp ties.
- A6 (2026-10-04): Late data is marked backfill on the same channel. The writer must
  label a batch backfill (unlabeled backwards samples still rejected). A backfill
  batch is internally ordered and ends before the newest live sample. Live readers
  never see it; recording readers (sinks) get it marked and write into the past.
  Sequence numbers count batches in arrival order. Sparkplug `is_historical`
  precedent. Rejected: drop, any-order, hold-and-merge, separate channel.
- Vocabulary (user, 2026-10-04): **sample** (one value at one time), **series** (an
  array of samples for one channel, one type), **frame** (series for several
  channels, keyed by channel, sent together). Never "batch".
- A7 (2026-10-04, replaces the per-batch time-quality header): Index channels, as in
  Synnax. An index channel holds timestamps; each data channel points to one index;
  dozens of channels sharing a clock store and send timestamps once. Time quality
  belongs to the index (the index is the clock). Fixes over Synnax: (1) alignment only
  inside one frame (value i of a data series matches timestamp i of the index series
  in that frame), never storage position; (2) a frame may carry any subset of an
  index's channels, a missing channel has no samples at those times (Synnax required
  all-or-nothing, cesium/writer_stream.go:813, which broke Arc); (3) a channel naming
  no index gets a private index automatically. An index and its channels have one
  home and one writer in control; channels needing separate control get separate
  indexes.
- A8 (2026-10-04): Each index has a sequence counter (`seq`, one u64, no epoch) over
  samples (timestamps), not frames. A stateful codec omits it from frames that follow
  the previous one; sends it only on jumps (resume, gap, home move). Two counters per
  index: live and backfill (revises A6: backfill numbered apart), each gapless on its
  own path; recording readers follow both. Restart continues from the last seq on
  disk, which requires complete-mode readers to get frames only after they are on
  disk (disk-buffer decision). A home with a standby checks in with voters regularly
  (no two homes) and each check-in reserves the next large block of seq; a new home
  starts at the next block; codec marks the jump. No-standby homes need no blocks.
  u64 lasts 584,000 years at 1 MHz and stays under 2^53 for 285 years (JSON-safe).
  Name "sequence" (TCP, QUIC, NATS JetStream, Sparkplug); rejected alignment, offset,
  position.
- A9 (2026-10-04): Primitive base types: bool; i8-i64; u8-u64; f32, f64; timestamp
  (A5), duration (i64 ns); string (UTF-8), bytes; uuid. Numeric series are plain
  fixed-width arrays (zero-copy to NumPy). Left out for now: f16, 128-bit ints,
  decimals, JSON type. Enums and structs build on top (user: required).
- A10 (2026-10-04, accepted in principle): Struct types: named fields of primitives,
  enums, or nested structs. A struct channel is one channel, one index, one home, one
  gate. One series per channel; a struct series holds one array per (leaf) field,
  Arrow struct layout; codec treats each field array like a primitive series;
  subscribing to `channel.field` returns a plain series (A3). Struct samples are
  whole (optional fields a later decision). Row sinks assemble samples at the edge.
- OPEN (user, 2026-10-04): clarify every data structure precisely in a later pass
  (sample, series, frame, and the arrays inside a series; user asked "so we're adding
  a new data structure called array?").
- A11 (2026-10-04): Enums: named type with a fixed integer type and explicit value per
  variant (`enum Mode: u8 { off = 0, ..., fault = 255 }`). Samples store only the
  integer. Open: unknown values pass through flagged unknown, never dropped or
  errors. Adding a variant is safe; renumbering/removing breaks. Variant names are
  not part of channel identity (Sift lesson). Matches OPC UA EnumValues and DBC value
  tables. Stores without enums (InfluxDB) get integer, name, or both per integration
  setting.
- A12 (2026-10-04): `flags` type: unsigned integer with named bits and multi-bit
  fields (a field can read as an enum). Samples store the raw word (exact round-trip
  for commands). Named bits addressable with dots (`pump_1.status.fault` -> bool
  series). Unnamed bits pass through. Use flags when the source is a packed word,
  struct of bools otherwise. Matches OPC UA OptionSet, Sift bit fields, DDS bitmask.
- A13 (2026-10-04; user may revisit bounded lists): Fixed arrays `T[N]` and
  multi-dimensional `T[N][M]` (row-major), and bounded lists `list<T, max>` (max
  required). Elements any type. Columnar: `f32[64]` x 100 samples = one 6,400-value
  array; lists add a lengths array. Limits let nodes size buffers and reject
  oversized samples at the edge; `foundation discover` proposes limits for sources
  without one.
- A14 (2026-10-04, "for now"): Optional struct fields (`optional<T>`), one validity
  bit per sample (Arrow); required unless marked. Unions deferred: no first-wave
  source except rare OPC UA unions, every sink needs a mapping, adding later breaks
  nothing.
- A15 (2026-10-04): Types are defined as code (user: "definitions as code is
  great"). Each type version has a fingerprint (hash of shape: fields, types, enum
  values; not comments); wire sends it once per connection then a short number;
  linked meshes compare fingerprints. Fields matched by name; renamed fields keep an
  alias. Changes sorted before they take effect: safe (add optional field, add enum
  variant, widen number, raise list limit) vs breaking (remove/retype required
  field, renumber enum; lists affected readers; needs explicit opt-in). Readers decode
  with the writer's type then convert (Avro/DDS/Kafka schema registry).
- TOOLING NOT YET AGREED (user, 2026-10-04: "I don't know what a mesh file is ...
  discover is or plan is"): "mesh file", `discover`, `plan`, `apply` are placeholders
  from my proposals; design them with the user in the definitions-as-code section.
  A2 and A15 are agreed at the concept level only for these terms.
- A16 (2026-10-04): Units only, no ranges. A unit (e.g. `kPa`) lives where the number
  is defined: on a primitive channel, or on a struct type's field (no per-channel
  override). Referenced by name; Foundation maps common units to standard codes
  targets need (OPC UA). Ranges dropped (user: a range is a validation rule, a rules
  concern; Foundation runs no rules); an integration that needs one (OPC UA EURange)
  holds it in its own config.
- A17 (2026-10-04): Scope addition: Foundation has **calculated channels** (a
  calculation engine). Calculations write only data channels, never commands, so D2
  (no control logic) holds. Scaling is a calculation (e.g. `pt_101 = pt_101_raw *
  0.0305 + 1.2`); where it runs is a placement choice: on the DAQ node (send
  engineering values) or past a weak link (send raw, scale on arrival). Same engine
  covers averages, unit conversion, downsampling. Arc: no shared code, but take
  inspiration freely (user). Engine design (language, placement, windows, state) is
  its own section after connectors. Prior art: Ignition expression tags, Kepware
  Advanced Tags, Vector VRL, Synnax calculated channels.
- A18 (2026-10-04): Quality ships in v1 and is saved in the format. Per sample,
  optional side array (absent = all good); OPC UA 32-bit status codes (lossless OPC
  UA; Sparkplug, Ignition, Modbus, DAQmx map in); run-length encoded; calculations
  propagate worst input. Foundation never judges quality itself. Format rule: series
  carry optional side arrays (validity bits A14, list lengths A13, quality) that
  readers can skip if unknown. OPC UA connector carries quality first.
- A19 (2026-10-04): No JSON type. `bytes` is plain bytes with no label (user: a
  label is metadata pretending to be a type; nothing enforces it). Integrations keep
  any forwarding/encoding info in their own config. The way to structure data is a
  struct; Foundation can propose one from sample payloads.
- A20 (2026-10-04): No command channel type; all channels are the same, differences
  come from settings. Retention (channel): whether/how much the home keeps a disk
  buffer. Mode (reader): `complete` (every sample, replayed after outages) or
  `latest` (newest only). Max age (reader): reader skips samples older than this; a
  "deadline" is just a latest reader with a max age (executing connectors must set
  one; whether a channel can carry a default is open). Writes to the home are never
  buffered for any channel (A1). Ack channel has the same type as the command; the
  connector writes the applied value; failures use quality codes (A18); writer
  matches by "next ack after my write" (one writer in control; confirmers wait per
  command); gate rejection is the direct reply to the write. A recorder reading
  command + ack channels is the audit log. Device state is a separate channel read
  back from the device; never echo commands as state (Synnax
  driver/common/write_task.h:72). Example: `valve_1.cmd bool retention none`,
  `valve_1.ack`, `valve_1.position`.
- DATA MODEL SECTION COMPLETE (A0-A20).
- B1 (2026-10-04, "tune later"): Disk buffer at the home for channels with retention.
  Keeps everything a durable reader (defined as code, e.g. an InfluxDB sink) has not
  received, within one disk budget per node shared by its channels. Ad-hoc readers
  never hold data. When full: oldest data goes first, affected readers get an
  explicit gap; node warns early on its status channel, naming the reader holding
  the buffer. Group commit every few ms; complete readers get frames only after
  they are on disk (A8); latest readers get them immediately. JetStream interest
  retention + size cap.
- B2 (2026-10-04): Subscription = channels (names or patterns; patterns stay live as
  matching channels are created), mode (complete | latest), from (resume | now |
  seq | time), optional max age. Durable readers have a name, are defined as code,
  resume where they stopped, and hold data in the buffer. Ad-hoc readers have no
  name, start from now/seq/time, hold nothing. A start time maps to the first sample
  at or after it per index; explicit gap if the buffer no longer has it.
- B3 (2026-10-04, may be redesigned later; user: "I do really care about performance
  a lot"): Complete mode: order per index only (separate indexes = separate clocks;
  SDK merges by time). Readers report one cumulative number per index; durable
  readers report after safely storing. Credit-based flow: a slow reader reads from
  the disk buffer and catches up; never slows writers or other readers; never
  dropped (only B1's budget limits lag). At-least-once; integrations make writes safe
  to repeat; seq makes repeats easy to spot.
- B4 (2026-10-04): Latest mode: new reader gets the current value immediately (home
  keeps the newest frame per index in memory, even with no retention; skipped if
  older than max age). Slow reader skips frames: at most one waiting frame per index
  per reader, newer replaces it; frames never split. Skips visible via seq jump. No
  replay after disconnect. Fast path: sent before disk sync, never read from disk.
  DDS KEEP_LAST depth 1, OPC UA queue size 1. Configurable depth can come later.
- P1 (2026-10-04; user: "Foundation needs to absolutely and totally fucking rip"):
  targets, benchmarked on a dedicated machine, merge blocked on >5% regression:
  100M numeric samples/s per node (disk buffer + one complete reader; Synnax peaked
  64-93M in the one-billion-rows blog); within 2x at 100k channels (Synnax degraded
  with channel count); latest-mode p99 < 250 us one LAN hop encrypted (1 kHz loop
  budget); < 4 bytes/sample typical sensor data incl. timestamps; runs on Raspberry
  Pi 4 1 GB, idle < 50 MB, start < 1 s. Zenoh reference: 4M msg/s 8-byte, 16 us avg
  multi-machine (zenoh.io 2023-03-21 blog).
- TESTING IS BEDROCK (user, 2026-10-04): extremely sophisticated harness and
  strategy: deep unit tests, deep isolated component benchmarks, end-to-end
  performance tests on shared infrastructure, and the Synnax HITL (self-hosted
  `ubuntu-test-bot` / `windows-test-bot` runners, .github/workflows/
  test.integration.worker.yaml). Testing section (T) runs now, before finishing
  delivery, because it constrains every component boundary.
- T1 (2026-10-04, "absolutely"): Injection rule: every component receives clock,
  network, disk, randomness as inputs (real in prod, simulated in tests); Foundation's
  own Transport trait in front of iroh (no sim clock in iroh). Layers: (1) unit +
  property tests, every commit; (2) fuzzing (user addition): coverage-guided
  (cargo-fuzz) on every decoder of outside input (wire, config, codecs, connector
  protocol parsers), short run per merge + continuous nightly, crashes become
  permanent regression inputs (Synnax precedent fuzz.go.yaml); (3) deterministic
  simulation of a whole mesh (drops, partitions, crashes mid-write, clock jumps;
  a recorded random value replays the run), thousands of simulated runs per merge, millions nightly; (4) unit-level
  benchmarks (user addition: "critical"), per function; (5) component benchmarks;
  both on the dedicated machine, 5% gate every merge; (6) end-to-end performance on
  shared infrastructure vs P1, nightly + release; (7) protocol simulators per
  connector, every merge; (8) Synnax HITL with real NI/LabJack/PLC, nightly +
  release. Candidate for later: mutation testing (cargo-mutants --in-diff) to check
  agent-written tests.
- T2 (2026-10-04; user: "I don't think we need to be THAT strict"): Oracles agreed in
  principle: simulation invariants, P1 targets and benchmark baselines, conformance
  suites, fuzz inputs (agents add, never remove) are person-owned; agents write most
  tests and add tests anywhere. Evidence: Carlini C compiler (GCC torture), Bun port
  (own suite, 0 skipped), Cursor browser (weakest oracle, weakest result). Proposed
  enforcement (local hook blocking agent edits + CI person approval on oracle paths)
  is TOO STRICT per user; the enforcement level is a parameter for the factory
  section. Do not re-propose the full lockdown.
- B5 (2026-10-04): Live writes never wait: home always passes live frames to latest
  readers; when its disk queue is full it stores an explicit gap ("N samples missing")
  instead; complete readers see the gap; node warns on status channel naming
  channels. Backfill writes wait for room (lossless). Rejected: live writes wait
  (Kafka producer / reliable DDS): for hardware the loss just moves to the device
  buffer and live readers stall.
- B6 (2026-10-04; user: should be tunable, "play with different transmission and
  acquisition settings"): One write call = one frame. Default transmission packing is
  smart batching (no timer: idle link sends at once; while a send is in flight, frames
  queue and leave together; Zenoh-like). Complete readers catching up from disk get
  consecutive frames merged (frame boundaries carry no meaning). Settings as code,
  changeable on a running mesh: acquisition per connector task (sample rate, samples
  per read = frame size); transmission per link with per-index overrides (`linger`
  default 0, max packet size, compression level). End-to-end perf suite (T1 layer 6)
  sweeps settings to pick defaults from measurements. Channel priority on shared
  links: wire section.
- B7 (2026-10-04, starting point): frames applied whole or not at all; live writes
  never retried (error per unconfirmed frame); backfill uploads number frames and the
  home drops repeats (Kafka idempotent producer); a writer may resend unconfirmed live
  data as backfill. Connector channels default to a home on the connector's node.
- RESCOPE (user, 2026-10-04): "all of it can be tuned through experimentation ... I'm
  interested in core data structures, configuration as code, ... architectural
  boundaries and different components. A lot of this stuff is internals that we can
  play with." B1-B7 are starting points, not interview items. Remaining delivery and
  wire internals (codec details, routing, liveness, packing, priority) are tuned by
  benchmarks (T1), not interviewed. Interview only: data structures, config as code,
  component boundaries.
- C1 (2026-10-04; "fine start"; user unsure about the name `hub`, won't push): Rust
  crates in four layers, each depends only on lower layers. (1) Data: `types`,
  `codec`; no I/O, no clock. (2) Core: `buffer`, `home`, `transport`, `mesh`, `time`,
  `hub`. (3) Edges: `connector-*`, `integration-*`, `calc`. (4) Surfaces: `config`,
  `cli`, `mcp`, `node` (wiring). Rule 1: layer 3 uses only `hub`, the in-process
  reader/writer API: plain function calls, frames by reference, no copy, no encoding
  (user: connectors must not go over the network to the node; "acts like a client but
  it isn't actually a client"); `hub` uses `transport` only when a home is remote.
  Rule 2: only layer 2 does I/O, receiving clock, network, disk as inputs (T1). SDKs
  (Rust, Python) live outside the binary and speak the wire protocol; the Rust SDK
  reuses `types` and `codec`. Rejected name `bus` (means fieldbus to users). Name
  `hub` is a parameter.
- S1 (2026-10-04; user rejected my per-index frame: "The frame should not need to
  carry information that should be known and/or preloaded by both ends ... Synnax's
  model is better"): `Frame { keys: Vec<channel::Key>, series: Vec<Series> }`, one
  series per key, may mix any number of indexes. Time is just the index channel's
  series. Series on the same index have equal length (A7 alignment). Path
  (live/backfill) belongs to the writer, fixed for the stream's life, not the frame.
  Seq moves to the series (like Synnax Alignment). Cost: a frame spanning two homes
  can be partly stored; B7 whole-or-nothing holds per index. Design principle: never
  put in a frame or series what both ends already know from definitions.
- PERFORMANCE PRINCIPLES (user, 2026-10-04, rejecting my Arrow-layout S2): "a
  single channel series should send ONLY its buffered data over the network as the
  payload. that's it. no other info. the other two sides are already aware." Internal
  architecture must MINIMIZE copying, locking, and heap allocations "at all costs".
  Storage format "deeply oriented around performance". Own layout is a key advantage;
  Synnax's layout was "really powerful and capable". No Arrow, no arrow-rs.
- S2 (2026-10-04, revised; user: "using arrow is a cop-out"): `Series { seq: u64,
  data: Buffer }`; seq in memory only (codec predicts it on the wire, A8). Wire
  payload is only the data bytes; type, sample width, compression known per channel;
  fixed-width sample count = bytes / width. Every type fits one buffer at computable
  positions: fixed-width struct = field columns back to back, no headers. Strings,
  lists, optional fields: offsets/presence bits inside the buffer (S3; Synnax newline
  separation breaks for binary). QUALITY IS ITS OWN CHANNEL (e.g. `pt_101.quality`),
  REVISES A18 side array. Memory, wire, disk share one byte format where possible
  (home writes as received, serves catch-up from disk without re-encoding; Kafka);
  re-encode only for links with different compression. Buffers from a preallocated
  pool; network reads straight into pool buffers; no heap allocation on hot path.
- C2 OPEN (2026-10-04; user: "mark this as a question to resolve, and then come back
  to it later"): proposed shard per core owning indexes (no locks; lock-free queues;
  per-shard disk files and group commit; network + connectors on Tokio; Redpanda,
  ScyllaDB, TigerBeetle). Research was shallow. Before locking: deep research (iroh
  on per-core runtimes; cross-platform thread-per-core runtimes, io_uring Linux-only;
  blocking vendor libs like DAQmx in shards; measured TPC vs work-stealing) plus a
  local benchmark (Tokio -> pinned shard handoff vs work-stealing + locks). Web
  search budget hit 200/200 this session; user will raise
  CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION.
- AGENDA (2026-10-04; user: work through a list of questions, then a deep research
  pass as part of iteration). [R] = needs the research pass before locking.
  Data structures: S3 [R] variable-length and optional layouts in one buffer; S4 [R]
  on-disk storage format and codecs; S5 channel definition; S6 index definition; S7
  type definitions and references; S8 node; S9 mesh state (what voters agree on);
  S10 reader and writer definitions; S11 control (authority, lease, control-state
  channel). Config as code: K1 [R] definition language/format; K2 file layout and
  composition; K3 workflow commands (discover/plan/apply naming, drift); K4 secrets;
  K5 linked meshes. Boundaries: C2 [R] thread model; C3 connector contract; C4
  integration contract; C5 [R] calculation engine and language; C6 time sync role;
  C7 agent surfaces (CLI, MCP, status); C8 identity and permissions; C9 factory (repo
  layout, oracle enforcement level, review rule, release).
- S5 (2026-10-04): `Channel { key, name, kind: Kind }`; `enum Kind { Index { home,
  standby, retention, ...S6 }, Data { index: channel::Key, data_type, unit } }`. An
  index IS a channel (key, name, in frames, readable). The index owns where and how
  long (home, standby, retention); its data channels share them. Data channels only
  describe meaning (type, unit, which clock). No calculated or virtual flag
  (calculation = writer naming its output, C5; virtual = index with no retention).
  Enum, not is_index bool. Synnax puts IsIndex, LocalIndex, Leaseholder, Virtual,
  Expression on every channel. Example: `stand_3.time` (Index), `stand_3.pt_101`,
  `stand_3.tc_204` (Data on stand_3.time).
- Agenda confirmed (user: "Great, let's do it"). [R] items wait for research pass.
- S6 (2026-10-04, "generally ok"; settings moved out by S12): `Kind::Index` carries no
  placement/retention fields. Rules: timestamps strictly increase per path (A6); no
  rate field (rate is a connector acquisition setting, B6; codec detects fixed
  rates); time error bound (ns) lives in a channel the index points at (revised
  after S13: explicit `error: Option<channel::Key>` pointer, not an auto-created
  `<index>.error` by naming convention). Evidence: DAQmx t0 + n*dt (~4 s/day at 50
  ppm), LabJack T7 up to 10.4 s/day, adjtimex max error, AWS ClockBound.
- S12 LOCKED (2026-10-04, after revision; user: "does a channel have a standby policy, or do we
  apply a standby policy to a set of channels. Same with retention ... think
  carefully about relationships and dependency in data structure directions ...
  what's flexible and powerful with the end user"): proposed policies as separate
  objects selecting indexes by name pattern; separate kinds (placement, retention,
  transmission B6, access C8); most specific pattern wins, equal specificity is a
  plan error; explain command shows effective settings; names become selectors
  (rename can move a channel under other policies; plan shows it). Prior art: S3
  lifecycle by prefix, Elasticsearch ILM by index pattern, JetStream streams by
  subject, Kubernetes selectors. One shared `Selector { patterns: Vec<Pattern> }`
  for subscriptions, durable readers, integrations, policies, permissions (one
  matcher, one wildcard rule set). Config shape: `[[retention]] select = "site_a.**"
  keep = "3d"`, `[[placement]] select = ... standby = "gateway_b"`. Policies apply to
  whole indexes; data channels follow their index.
- S13 (2026-10-04; user's idea: "you can have one quality channel for many different
  channels"): `Data { index, quality: Option<channel::Key>, data_type, unit }`.
  Quality channel has type `Quality` (plan error otherwise), its own index, written
  only on change; a value holds until the next one (step); one quality channel can
  serve many channels (whole PLC goes bad on comm loss); per-sample quality = a
  quality channel that changes every sample. Integrations match by time (as-of join,
  pandas merge_asof). REPLACES A18 side array entirely. User: "Structurally
  simplifying the model was a huge advantage."
- MODEL MAP (dependency direction; keep current as design proceeds):
  Data channel -> index channel, -> quality channel (optional), -> type definition,
  -> unit. Index channel -> error channel (optional). Type definition -> other type
  definitions (struct fields); types never -> channels. Policies -> channels via
  selector; channels never -> policies. Durable readers, integrations,
  subscriptions -> channels via selector. Calculation -> input channels, -> one
  output channel; channels never -> calculations. Connector -> channels it writes;
  channels never -> connectors (home defaults to the connector's node, B7).
  Simplification pattern: anything that changes over time is a channel (quality,
  clock error, control state, acks) instead of a field or side array. Voters ->
  branches of the name tree via selector (K5); nodes, connectors, and channels all
  live in the one name tree.
- S7 (2026-10-04; user: "might be some gaps in the optional fields piece but yes
  generally I agree"): A STRUCT IS A TEMPLATE OF CHANNELS. `motor_1: MotorState`
  expands to `motor_1.speed`, `motor_1.current`, ... one channel per field, same
  index. Runtime knows only channels typed primitive, fixed array, enum, flags,
  string, bytes. Revises A10 (columnar struct), A14 (optional field = channel absent
  from the frame, A7), A15 (field add = channel add; rename = channel rename, key
  stable A4; fingerprints only for enums and flags), S2 struct layout (gone; S3 now
  only strings, bytes, lists). Per-field subscribe, quality, policy for free; array
  of structs = one array per field; typed SDK views generated from definitions
  (protobuf-style). Prior art: Ignition UDT instances (a tag per member), Kepware
  flattens structures. Trades: home cannot check that required fields arrive
  together (generated writers always send them); channel counts grow (1,000 motors x
  20 fields = 20,000 channels), so per-channel cost must be tiny (S4 research).
  OPEN GAP (optional fields): "absent" is indistinguishable from "not written"; a
  latest reader assembling a struct can pair a new `speed` with a stale `fault` from
  an earlier sample (as-of pitfall). Resolve with S3 or a follow-up.
- S8 (2026-10-04, "for now"): `Node { key: node::Key (UUIDv7, stable), name (e.g.
  "site_a.gateway_1"), public_key (Ed25519 iroh identity, rotatable) }`. Stable key
  separate from crypto identity (Tailscale stable node ID vs expiring node keys). No
  role fields: voters listed at mesh level (D7), time role chosen the same way (C6).
  Everything that changes about a node is a channel under its name
  (`site_a.gateway_1.disk.used`, `.clock.error`); a node name cannot be a channel
  name. Node lists no channels/connectors; connectors and placement point at nodes.
  iroh = candidate network library (n0; QUIC dial-by-key, NAT traversal, relays),
  not locked.
- S9 (2026-10-04, revised after user questions on ownership, Raft vs Paxos, gossip,
  "the mesh spec could turn out to be extremely large"): `State { spec, runtime }`;
  spec changed only by `apply`, runtime (homes per index, leases per node) only by
  the mesh; plan compares files with spec only (failover never drift; Kubernetes
  spec/status). CONSENSUS ON A POINTER: Raft holds only the current spec version +
  hash and runtime state. Spec is a content-addressed tree following the name
  hierarchy (Git-like); a change rewrites only its branch; nodes fetch changed
  branches, verify by hash, from any node that has them; a node fetches only the
  branches it uses. Iceberg atomic pointer swap, Git refs vs objects. `mesh.changes`
  built-in channel carries the small records; owned by the voters together; seq =
  Raft log index; any copy can serve it; readers resume from any source (KRaft
  metadata log). Leases per node; on lapse each index moves to its placement
  standby. Fast state out of Raft (status, health, clock error, control state are
  channels; reader positions at the home). Gossip only for hints (liveness,
  addresses, load), never ownership; Synnax Aspen homegrown gossip never shipped
  multi-node.
- LIBRARY RULE (user, 2026-10-04): "be very careful about which 3rd party libraries we
  use. If we really need to implement our own production grade RAFT or gossip we can
  do it ... make sure we're considering what the best architecture is." Architecture
  first, then library vs own implementation, for every major dependency. Filter: a
  library that does its own I/O or reads the clock breaks DST (T1); sans-I/O cores
  (raft-rs, quinn-proto style) pass. Candidate principle for the research pass: core
  protocols as sans-I/O state machines with thin I/O drivers.
- RESEARCH PASS ADDITIONS: consensus algorithm and build vs adopt (openraft, raft-rs,
  own); gossip need and build vs adopt; iroh vs own transport; build-vs-adopt audit
  of every major dependency; sans-I/O principle.
- S10 (2026-10-04, revised; user: "their just readers ... two things reader and
  writers with different policies"; no durable vs ad-hoc split): `Reader { name:
  Option<String>, select: Selector, mode, from (now|oldest|seq|time|resume), max_age,
  hold: Duration (default 0) }`. Script = defaults; integration = name + hold 7d.
  Only complete mode can hold. Hold capped by the index's retention policy (a reader
  that never returns stops holding; removes B1's trade). Readers are NOT defined as
  code: a reader is a session, lasting while it runs and then until its hold ends.
  Defined as code: integrations (contain reader settings) and retention policies (cap
  holds). Current readers and their holds are status channels. One session attached
  per named reader; new one takes over (consumer-group split later). Positions at
  each home per reader and index. Writers: one concept, a session `{ subject,
  authority, lease, path, channels }`.
- S11 (2026-10-04, revised after "Why do you need lease length?"): the home's gate is
  the only enforcement point; executors never check control (Synnax treated unknown
  as in control, driver/control/state.h:141). Control state is a channel the index
  points at: `Kind::Index { error: Option<channel::Key>, control:
  Option<channel::Key> }`, written only by the home once per handoff (holder subject
  + authority); B4 gives cold subscribers the current holder (fixes RFC 0057); no
  value = unknown = not in control. Authority requested at writer open, capped by
  access policies (C8); 255 cannot be taken (Synnax AuthorityAbsolute). Lease is an
  optional WRITER setting (`lease: Option<Duration>`), not a policy: none = control
  lasts while connected; control loops set short leases (catches a frozen but
  connected holder). No `[[control]]` policy kind. Higher authority can always
  override a stuck holder.
- DATA STRUCTURES DONE except [R] S3, S4. Config order: K3 workflow, K4 secrets, K5
  linked meshes; K1 language and K2 file layout go to the research pass (candidates:
  TOML/YAML + conventions, Pkl, CUE, KCL, Nickel, Starlark, HCL, Jsonnet, own DSL,
  SDK code like Pulumi/CDK; must express templates, selectors, typed connector
  config, calculations).
- K3 (2026-10-04): files in Git = desired mesh; spec (S9) = running mesh. Commands:
  `discover <device>` (writes definitions into files; review the diff), `plan`
  (files vs spec: channels added/removed, settings each policy change moves, breaking
  type changes, affected readers), `apply <plan>` (commits exactly the reviewed plan;
  refuses if spec changed since; compare-and-swap on S9 pointer), `explain <name>`
  (effective settings + which file/policy set each), `export` (running spec ->
  files, to adopt a mesh). Only apply changes spec, so no silent drift; A2 open
  folders' runtime channels listed by plan until added or deleted. Every command has
  `--json` with stable change kinds. Terraform names (OpenTofu same; Pulumi same model).
  These resolve the earlier "TOOLING NOT YET AGREED" placeholders except the file
  format/name (K1, K2).
- AGENT REQUIREMENT (user, 2026-10-04): "make sure it's extremely easy to use agents
  to do all of this for you." Carry into C7. Candidate ideas: MCP tools for every
  command with annotations (apply destructive); `init` writes an agent guide/skill
  into the repo; JSON Schema for the config language; errors with fix-it hints; docs
  embedded in the binary; try a plan against a simulated copy of the mesh (reuse the
  T1 simulator) before apply.
- K4 (2026-10-04): config refers to secrets by name only (`token = { secret =
  "influx_token" }`); values never in files, plans, or output. Write-only: `foundation
  secret set <name>` / delete, never read back (GitHub Actions secrets). The mesh
  encrypts each value to the nodes that use it; voters store ciphertext only (Bitnami
  Sealed Secrets). Node-side sources too: env var or file (Vault, Kubernetes users).
  `plan` checks every reference resolves, reports missing by name. Agents wire
  references and run plan/apply but never see values; a person or CI sets them.
  Open detail: re-encryption on node key rotation (S8).
- K5 LOCKED (2026-10-04; cross-company sharing deferred, user: "we can handle cross
  company sharing later"): first proposal (separate meshes + export/import, NATS
  accounts and leaf nodes) accepted in substance, but user: "we should be extremely
  careful about making this as simple as possible ... instead of using complicated
  extra terminology sites, and links and linked meshes ... keep the model as simple as
  possible ... we definitely need to support all of this." Re-proposing: ONE mesh;
  voters are a policy that selects a branch of the name tree (DNS zone delegation);
  each branch's voters agree on that branch's definitions (S9 pointer per branch);
  cross-branch access is ordinary access policy (C8); no site/link/export/import
  terms.
  Locked shape: `[[voters]] select = "site_a.**" nodes = [...]`; default `select =
  "**"`. A site keeps changing its own branch while cut off. Node names are in the
  tree, so site nodes are governed by site voters. Trades: cross-branch change
  commits in two steps; changing a branch's voters needs the parent branch's voters;
  cross-company sharing later. Revises D7 "linked meshes": there is one mesh.
  SIMPLICITY DIRECTIVE (user): keep the model and its vocabulary minimal; express
  new needs with existing concepts (policies, selectors, channels) before new terms.
- CONFIG SECTION DONE except [R] K1, K2.
- C3 (2026-10-04): ONE CONCEPT, THE CONNECTOR: connectors and integrations merged
  (C4 removed; "integration" is no longer a term). `[[connector]] name =
  "site_a.plc_7" kind = "opcua" node = "site_a.gateway_1" url = ...`, with
  `[[connector.in]]` (device -> channels; a writer session) and `[[connector.out]]`
  (channels -> device or store; a reader session: commands latest + max age, stores
  complete + hold; stores select many channels by selector). Named in the tree
  (governed by the branch's voters, K5). One connector = one outside endpoint owned by
  one task (no shared handles; Synnax LabJack handle race). Changing state = channels
  under its name (`site_a.plc_7.status`). Contract per kind: typed config with schema,
  `run`, optional `discover`, one shared error set (retry, config error, device
  fault). Kinds in one explicit compiled-in table (D4). Prior art: Kafka Connect
  (source/sink), Telegraf, Redpanda Connect.
- C5 SHAPE (2026-10-04): a calculation is a connector kind (`kind = "calc"`, `expr`,
  `node` = where it runs, A17 placement). Gets lifecycle, status channels, error set,
  table slot, future plugin slot (D5) for free. Expression language stays [R]. Name
  stays "connector" (user floated "integration"; rejected because it collides with
  integration tests: `integration/`, test.integration.yaml, Rust `tests/`).
- C6 (2026-10-04; user: "make as much of this setup as automated as possible"): every
  node keeps its own mesh clock = OS clock + measured offset, with an error bound
  (interval earliest..latest, TrueTime/ClockBound). Never changes the OS clock by
  default (no privileges; same on Linux, Windows, macOS; W32Time 1 ms best).
  NTP-style timestamp exchange over the mesh; publishes `<node>.clock.offset` and
  `.clock.error` channels. Time source is a policy: `[[time]] select = "site_a.**"
  source = "site_a.gps_1"`; default = the voters of the node's branch (a remote site
  syncs locally). GPS/PTP hardware used directly for tighter bounds; OS steering
  opt-in where privileged. All timestamps in mesh time; connectors convert device
  time (S6). Trade: without OS steering other software sees slightly different time.
  [R]: accuracy per source; own PTP client or not. AUTOMATION REQUIREMENT: detect
  GPS/PTP hardware and pick sources automatically; zero time config by default.
- C7 (2026-10-04, revised; user: "we might need to do tuning and high level work on
  each SDK to get it as native as possible ... We can't just 'generate the SDK'"):
  one table of operations (typed input/output, error codes, read-only/destructive
  flags) fully generates the CLI (`--json`), the MCP tools (annotated), and the docs
  embedded in the binary (`foundation docs plan`). SDKs have two layers: a generated
  base (types, operation calls, errors) plus a hand-written native layer (Python:
  NumPy, pandas, context managers; Rust: iterators, zero-copy frames); the data path
  (frame reader/writer) is always hand-written per language (Python series = NumPy
  array over the received buffer, no copy). Shared test suite generated from the
  table checks every operation in every SDK. Google Cloud libraries, boto3. Also:
  every error has a stable code + fix-it hint; status is channels (no status API;
  Prometheus/Grafana via out connectors); `foundation init` writes an agent guide
  into the repo; `plan --simulate` runs a plan on a simulated copy of the mesh (T1
  simulator + protocol simulators).
- C8 (2026-10-04, after user asked "Why is this the right approach compared to other
  access models? What about restricting access to specific channels?"): a subject is
  anything that reads or writes (person, agent, program, connector), named in the one
  tree (`people.alice`, `agents.ops`, `site_a.plc_7`), governed by branch voters (K5).
  People, agents, programs authenticate with keys (like nodes, S8); a connector is
  vouched for by its node. Access is an allow-only policy, default deny, no conflicts
  (Tailscale ACLs, NATS subject permissions): `[[access]] subjects = [...] select =
  ... allow = ["read","write","plan"] authority = 200`. Actions: read, write, plan,
  apply, secret, admin. No group/role concept: a group is a selector over subject
  names. A connector may write channels under its own name by default. An agent is an
  ordinary subject (per branch: plan only, or apply). SSO later (maps login to
  subject). Specific channels = selector without wildcards. Rejected: per-object ACLs
  (wrong dependency direction, unmaintainable at 20k channels/site), RBAC (roles +
  bindings; Synnax policies list objects by ontology ID or type, attached to roles,
  core/pkg/service/access/rbac/policy), ABAC/IAM (conditions + explicit deny
  precedence), Zanzibar/ReBAC (per-document sharing). `plan` lists access changes
  separately (rename can change access). SELECTOR AMENDMENT (S12): selectors support
  exclusions (`"!site_a.plc_7.valve_9.**"`), for every policy kind.
- C9a (2026-10-04): `foundation/` with its own Cargo workspace; `crates/` by layer
  (types, codec | buffer, home, transport, mesh, time, hub | connector,
  connector-opcua, connector-calc, ... | config, cli, mcp, node) + `sim/` (test-only
  simulated clock, network, disk); `sdk/python/`; `oracles/` (invariants, P1
  targets, conformance suites, fuzz inputs); `bench/`. Checks on every PR: layer
  check (crate depends only on lower layers, fix-it message; OpenAI harness lints)
  and stand-alone check (no dependency on paths outside `foundation/`). Monorepo fit:
  `foundation` path filter + own workflow gated by `OK`; `.bazelignore` entry; avoid
  dir names hidden by .gitignore (bin, data, gen, build, dist).
- C9b (2026-10-04): work loop: (1) person picks the next RFC phase, a planning session
  splits it into tasks (goal, tests that must pass, crates owned; no two tasks own
  the same crate at once; C1 layers make splits natural); (2) one implementing agent
  per task in its own Git worktree (Bun agents stashed/reset each other's work); (3)
  machine gates before review: build, lints, layer + stand-alone checks, unit +
  property tests, thousands of simulation runs, short fuzz, 5% benchmark gate,
  mutation testing on the diff; (4) two fresh adversarial reviewers see only the
  diff + its RFC section, findings back to the implementer (Bun); (5) person reads
  summary, spot-checks, merges; (6) cleanup agents. Flat peers with locks stall at
  2-3 agents (Cursor).
- QUALITY CREW REQUIREMENT (user, 2026-10-04): "define a strong set of agents that
  cleanup for code quality, inspect test issues, inspect breakages of architectural
  boundaries, test performance, etc. ... making sure that the agents doing the work
  are writing the code as cleanly as possible." Proposal C9b2 pending.
- C9b2 (2026-10-04): quality crew, six single-job agents: code quality (namespace
  rule, comments, dead code, duplication -> cleanup PRs); tests (crate-wide mutation
  testing, flaky tests, mocks instead of real deps, RFC invariants without tests ->
  tests); architecture (model-map direction, direct clock/network reads, globals,
  pass-throughs -> issues/PRs); performance (nightly benchmarks, bisect regressions,
  hot-path allocations/copies/locks -> issues with profiles); failure triage (nightly
  simulation + fuzz failures minimized -> regression tests + replay command); drift
  (RFC/docs/generated docs vs code -> PRs). Same gates + review as all PRs; can add
  tests, never weaken oracles; each has a person-owned rulebook in the repo that
  grows when a person catches a miss; one command starts the crew run daily (keeps
  the factory within attended local sessions).
- C9c (2026-10-04): oracles enforced by visibility, not blocking: no hooks, no extra
  approvals; a script writes an oracle section at the top of every PR summary
  listing changes under `oracles/` and flagging weakening (removed test/assertion,
  loosened threshold, raised benchmark baseline, deleted fuzz input); additions not
  flagged; every flagged change gets its own fresh ADVERSARIAL reviewer (user: "changes
  to oracles should also be checked adversarially by other agents") arguing for
  fixing the code instead, verdict at the top of the summary. A person merges every PR.
- C9d (2026-10-04): releases per RFC 0058 (dispatch from main or
  release/foundation-X.Y; tag `foundation/vX.Y.Z`; -rc.N never shipped). One binary
  per target (Linux x86-64 + ARM, macOS, Windows), each tested on real hardware via
  HITL runners. `foundation upgrade <version>` is an ordinary operation (agents can
  run it); rolling, one node at a time; nodes fetch the signed binary from a nearby
  peer by hash (like spec branches, S9). Every wire/disk format has one integer
  version; a node reads its own and the previous; new formats turn on only after
  every node runs the release (CockroachDB finalization; rollback possible until
  then). Compatibility owed only to stable releases. Trade: two-version support per
  format change for one release.
- INTERVIEW ROUND COMPLETE (2026-10-04) for all non-[R] items.
- RESEARCH PASS (next; waits on user raising CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION,
  hit 200/200): (1) C2 thread model + local benchmark (Tokio -> pinned shard vs
  work-stealing + locks); (2) S3 strings/bytes/lists/optional layouts in one buffer +
  the S7 optional-field gap (absent vs not written; stale as-of pairing); (3) S4 disk
  format, codecs (ALP, FastLanes, pco, timestamp stride), per-channel cost at
  millions of channels; (4) K1 definition language (TOML/YAML + conventions, Pkl,
  CUE, KCL, Nickel, Starlark, HCL, Jsonnet, own DSL, SDK code) + K2 file layout; (5)
  C5 calculation language (Arc ideas, VRL, Ignition expressions, Kepware advanced
  tags); (6) consensus algorithm and build vs adopt (openraft, raft-rs, own); (7)
  gossip need and build vs adopt; (8) iroh vs own transport; (9) C6 accuracy per
  time source, own PTP client or not; (10) build-vs-adopt audit of every major
  dependency + sans-I/O principle. After research: lock the [R] items with the user,
  then draft the RFC on a worktree from main.
- 2026-10-04: user said "Let's start forking deep research" and "go much more deeply
  through architectural boundaries". Gave /tmp/foundation-raise-search-limit.sh (sets
  env CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION in ~/.claude/settings.json, backs it
  up); user restarts with `claude --continue`. Restart kills background agents, so
  forks launch AFTER the restart. Fork plan (8, subagent_type fork, no sub-agents):
  (1) C2 runtime + local benchmark + sans-I/O; (2) S3 + S4 storage, codecs,
  per-channel cost, optional-field gap; (3) K1/K2 config language + C5 calc language;
  (4) consensus + gossip + spec tree distribution, build vs adopt; (5) transport,
  iroh vs own; (6) C6 time accuracy + PTP; (7) build-vs-adopt audit of all major
  dependencies (OPC UA, Modbus, MQTT, Kafka, NI, LabJack, compression, crypto); (8)
  BOUNDARY MAP draft (no web): per crate what it owns, its interface, its
  dependencies, invariants; end-to-end flows (write, complete read, latest read,
  failover, apply, discover, upgrade, command + ack); open boundary questions. Then
  a one-by-one boundaries interview from that map.
- 2026-10-04: search limit raised to 2000; all 8 forks launched. Reports land in the
  session scratchpad (/private/tmp/claude-501/-Users-emilianobonilla-Desktop-
  synnaxlabs-synnax/3200c2c6-b8bf-42cb-a9d0-9873249a055d/scratchpad/):
  foundation-r1-thread-model.md, -r2-storage.md, -r3-languages.md,
  -r4-consensus.md, -r5-transport.md, -r6-time.md, -r7-dependencies.md,
  -r8-boundaries.md. /tmp is wiped on reboot: copy key findings into this file
  when locking decisions.
- C3 REFINEMENT (2026-10-04, from "How do you define things like sample rates?"):
  sample rate lives on the connector, not the index (S6). A connector's `in` entries
  are grouped by index: one group = one clock = one index = one rate = one writer
  session (`[[connector.in]] index = ... rate = "10kHz" samples_per_read = 100
  channels = [...]`). `rate` = hardware clock (DAQ), poll rate (Modbus), requested
  sampling interval (OPC UA; server may revise, timestamps show reality). Different
  rates need different indexes (A7). Slower rate for a weak link = a downsampling calc
  (A17). No user-facing device or task: connector = device, groups = tasks;
  internally one actor per connector, one writer per in group, one reader per out
  group; connector kinds map groups to vendor concepts (NI: one DAQmx task per
  group). Synnax devices, racks, tasks collapse into connector (rack = node).
  Boundary questions added: runtime start/stop of one group without apply; two
  devices sharing one hardware clock writing one index.
- RE-INDEXING REQUIREMENT (user, 2026-10-04): "make re-indexing or changing index
  groups easy." A data channel can move to another index (rate change, split, merge)
  keeping key and name; stored data keeps its original timestamps (disk format
  records which index each stored range used); readers see one continuous channel;
  placement/retention follow the index, so re-index can move the home (history stays
  at the old home until retention ends; plan shows it). Sent to forks r2 and r8.
- R6 TIME RESULTS (2026-10-04, not yet locked with user): accuracy LAN NTP software
  timestamps tens of us; HW timestamps tens of ns; GPS+PPS on Linux ~1 us; W32Time 1
  ms best; LTE several ms; Starlink ~40 ms honest bound (asymmetry; dish has GPS NTP
  at 192.168.100.1). Build own mesh clock as a sans-I/O state machine over our
  transport; read GPS, PPS, NIC PHC, OS daemon directly as sources; no PTP client in
  v1 (ntpd-rs internals unstable/2.0 rewrite, steering no Windows; statime mostly
  unpublished; linuxptp/chrony separate processes, linuxptp Linux-only). Mesh clock
  on the raw oscillator; bound = half RTT per exchange (holds for any asymmetry);
  keep lowest-delay sample per source; combine with Marzullo; grow bound with drift;
  slew only. Auto: periodic source discovery + privilege check; follow the smallest
  measured bound, no fixed ranking; installer grants narrow Linux capabilities. Device
  clocks: drifting-oscillator fit to fastest read-return times; use device timestamps
  (DAQmx first-sample, LabJack CORE_TIMER); remaining error to the index error
  channel. Synnax driver/common/sample_clock.h:144-216 uses PC clock, biased late,
  no error.
- R4 CONSENSUS RESULTS (2026-10-04, not yet locked): Raft with PreVote + CheckQuorum
  always on; each voter group stays inside one network location (site LAN or one
  cloud region); votes never cross Starlink (reconfigures every 15 s); others only
  follow `mesh.changes`. EPaxos worst-case latency >4x Multi-Paxos. BUILD OWN sans-I/O
  Raft core modeled on etcd/raft (~5,600 lines Go core; its 28 test scenario files and
  TLA+ spec become oracles). openraft alpha (API + disk format churn); raft-rs no
  release since 2023, TiKV pins git master, hardcoded RNG at src/raft.rs:2857 breaks
  T1 (fallback: pin + patch). NO GOSSIP: leases, transport, status channels,
  mesh.changes cover it; Aspen suspect/dead states never set outside tests. Spec tree:
  one prolly tree per branch keyed by full name, ~4 KiB chunks, BLAKE3; each
  mesh.changes record lists new chunks (one round trip); a change rewrites ~16 KiB at
  1M defs, ~20 KiB at 10M; a tree copying the name hierarchy breaks on wide prefixes
  (20k-channel template). Branches: parent stores only `{branch, voters, epoch}`, never
  the child root hash; cross-branch change commits per branch in dependency order;
  forced takeover of a cut-off branch bumps the epoch. PROPOSED REVISIONS: K5 (a
  branch changes its own voters, so a cut-off site can replace a dead gateway); new
  rule for S5/S11/S12 (key references index, quality, error, control, home, standby
  stay inside one branch; cross-branch links only via selectors).
- R5 TRANSPORT RESULTS (2026-10-04, not yet locked): requirements hold with fixes:
  addresses come from the mesh, not DNS; a TCP path is mandatory (OT blocks UDP);
  latest mode drops stale frames via a stream per frame reset when replaced, plus
  datagrams for small frames; one QUIC connection is near P1 throughput limit;
  ChaCha20 on Pi 4; drop "every node can relay". Only noq-proto (n0's Quinn fork core)
  passes T1 with needed features (sans-I/O, `now` per call, fixed RNG, multipath, NAT
  traversal, datagrams, priorities); quinn-proto fallback (no multipath/NAT
  traversal). iroh `Endpoint` rejected (owns runtime, reads clock, #4459). Userspace
  QUIC: 2.4-8.2 Gbit/s per connection on 10 GbE, one-core bound; GSO/GRO + large
  packets required; Windows offloads unreliable; no published LAN p99, so C2
  benchmark must measure the 250 us target early. Relays inside designated Foundation
  nodes chosen by policy, TLS on TCP 443 + CONNECT proxy, admit only spec keys; try
  direct UDP -> direct TCP -> relay; no n0 infrastructure. One-way links (data diodes):
  separate small carrier: UDP + Noise K + RaptorQ FEC + seq + codec keyframes;
  complete mode becomes best effort with recorded gaps; commands, Raft, clock exchange
  cannot cross. Recommended: build own transport on noq-proto driven by shard loops;
  fallback: prototype on iroh behind the trait, switch before first stable.
- R3 LANGUAGES RESULTS (2026-10-04, not yet locked): K1: HCL syntax with Foundation's
  own semantics; files hold data only (no loops, for_each, modules); parse with
  `hcl-edit` (keeps formatting); own checker. Reason: discover/export must write files
  people then edit (round trip), which rules out Starlark, Jsonnet, Nickel, CUE, Pkl,
  KCL, SDK code; Pkl ~100 MB, CUE needs Go, Dhall Rust crate stale since 2022, no sound
  maintained Rust YAML parser; TOML has no expressions (calcs become unchecked
  strings). Precedent: Grafana River. K2: one directory per branch of the name tree
  (voters and code owners match directories); a file defines only names in its own
  branch; all references are full names; no imports; discover writes ordinary files
  and a rerun proposes a diff. C5: one typed expression per calc with unit checks;
  built-in windows (avg, min, max, derivative, every); as-of alignment (kdb+ aj);
  column-at-a-time over buffers; no loops, no user functions (bounded cost,
  deterministic). Rejected WASM (copy per batch), Lua/Rhai (interpreter step per
  sample), streaming SQL (too heavy for a Pi); Flux as cautionary tale. Q4: ONE
  LANGUAGE: calc expressions in the same files and checker (wrong unit/name fails at
  plan), holds only while a calc is one expression; larger logic -> WASM plugins.
- R7 DEPENDENCY AUDIT RESULTS (2026-10-04, not yet locked; binary size and idle memory
  unmeasured): OPC UA adopt async-opcua, fork only its crypto crate onto aws-lc-rs
  (RSA exposed to RUSTSEC-2023-0071, no fix, no ECC); client can plug its transport,
  server cannot (OPC UA server outside the simulator); open62541 as test peer. Modbus
  BUILD sans-I/O codec + serial2; rmodbus as comparison reference. MQTT + Sparkplug B
  BUILD sans-I/O; Cirrus Link patent license covers only TCK-passing implementations,
  so pass the TCK. Kafka BUILD pure-Rust client on `kafka-protocol` codecs (KIP-848
  broker-side assignment since Kafka 4.0 keeps the client small); rskafka has no
  consumer groups; alternative rdkafka behind a build flag. DAQmx + LJM BUILD runtime-
  loaded bindings (`ni-daqmx-sys` links at build time); Ethernet LabJacks via own
  Modbus + LabJack stream protocol. Codecs BUILD ALP, FastLanes-style bit packing,
  stride detection (spiraldb crates, pco as test references; own format versioning).
  TLS/crypto ADOPT rustls + aws-lc-rs only provider + blake3 (aws-lc the one shipped C
  library). Tooling ADOPT clap, schemars, toml/toml_edit, tracing; BUILD thin MCP
  server (rmcp had 20 incompatible release lines in 18 months); BUILD Prometheus
  text output. InfluxDB line-protocol writer BUILD; Ignition and Grafana need no
  library. Six user decisions listed in the report: OPC UA crypto, Sparkplug schema
  source, Kafka, NI header license, FIPS timing, MCP server.
- CANONICAL LIBRARY RULE (user, 2026-10-04: "Could also be worth it to build bindings
  to C libraries that we compile into the final binary ... we can use canonical C
  libraries that are production grade and tested for things like OPC UA, modbus"):
  per protocol, prefer the canonical production-grade implementation in any language,
  statically compiled into the binary with our own Rust bindings. Simulation is not a
  blocker: connectors reach the mesh only through `hub` (C1), so DST replaces any
  connector with a simulated one; C-backed connectors are tested by protocol
  simulators and HIL (T1 layers 7, 8). Hard tests: license allows static linking (BSD,
  MIT, MPL-2.0 fine; LGPL relinking duty awkward, e.g. libmodbus); library threads stay
  inside the connector's actor, never on a core shard. Likely: OPC UA -> open62541.
  Fork r7 resumed to re-check every area with this option (appends "Compiled-in C
  libraries" to its report).
  R7 C RE-CHECK RESULT: compiled-in C wins for OPC UA only: open62541 over
  async-opcua by a small margin (OPC Foundation certified Standard 2017 UA Server
  Profile valid to 2027-12-31; has ECC policies; its EventLoop takes our clock and
  network so the server can run in the simulator; against: 22 CVEs in 2026 incl. a 9.8
  and an OOB write in its core decoder; its OpenSSL plugin needs OpenSSL 3 APIs, aws-lc
  fit unconfirmed: write own crypto plugin on aws-lc or compile in mbedTLS; ~1.4 MB
  server, 0.27 MB client without crypto). No change elsewhere: libmodbus LGPL,
  nanoMODBUS blocks and had 5 CVEs in 2026; coreMQTT best C MQTT fit (we supply network
  and clock, no threads, 42 KB) but blocking connect and new MQTT 5 code; Paho own
  threads, Mosquitto own sockets; librdkafka thread per broker + reads system clock
  (1.47 MB minimal producer, same as "wrap"); DAQmx/LJM closed; zstd OK if wanted;
  libcurl own clock + 45 CVEs 2026. "Revised decision 1" (OPC UA) in report.
- R2 STORAGE RESULTS (2026-10-04, not yet locked): correction: Synnax moved off
  newline separation in SY-4059 to a u32 length prefix per sample. S3: variable-length
  series = `ends[n]` (u32 end offsets) + bytes; lists nest the same way; the frame
  carries the sample count once per named index group, so each series is only data;
  O(1) access, compressible offsets (Synnax prefixes make Len/At scan). S7 gap:
  presence is per frame; generated writer starts a new frame when the set of present
  fields changes; struct views built from one frame at a time, never from per-field
  latest values; disk chunks record which seq ranges they cover (absent field = one
  entry). S4: per shard two tiers: preallocated write-ahead ring (CRC32C per record,
  one group-commit sync, no directory sync on the hot path) + immutable columnar
  segments with one chunk group per index (Apache TsFile-like); eviction deletes whole
  segments; no per-channel files; disk cost per channel = one footer entry per
  segment; est. 100-200 bytes memory per active channel (benchmark owed); failed fsync
  is fatal, never retried. Cesium never fsyncs, no checksums, one dir per channel.
  Codecs: build rational stride (timestamps), ALP (floats), FastLanes (ints, offsets),
  RLE (step channels), same on wire and disk; ALP ~16.4 bits/value, decodes ~50x
  faster than Gorilla; pco and zstd optional behind benchmarks; <4 bytes/sample needs
  raw counts (A17). Re-index: committed epoch `(index, from: T)` on the channel; old
  home keeps samples before T, new home from T; no rewrite; mapping costs no bytes per
  chunk; one metadata write; ~16 bytes per epoch; possible gap at T bounded by clock
  offset.
- R1 THREAD MODEL RESULTS (2026-10-04, not yet locked; bench in scratchpad/tpc-bench/):
  each core's shard owns indexes with no locks; the connector writing an index runs on
  that index's shard; network frames handed to the owning shard once; hot indexes need
  a way to move. Evidence: Apache Iggy thread-per-core P99 4.52 -> 1.82 ms; PulseBeam
  P99.99 70 -> 10 ms; Redpanda; without.boats: uneven load favors work-stealing (main
  trade). Runtime: one Tokio `LocalRuntime` per shard (stable since Tokio 1.51.0, Linux
  macOS Windows); compio possible disk backend; glommio, monoio, tokio-uring
  unmaintained or Linux-only; no `tokio::fs`. iroh/noq run on single-thread runtime
  (iroh/src/endpoint.rs:3118; noq/src/runtime/tokio.rs:28-40); one endpoint per node
  on one network shard. Vendor libs: one OS thread per device handle making all
  blocking calls (Synnax driver/pipeline/base.h:23); LJM close from one thread closes
  for all; NI thread safety unverified. Sans-I/O: yes for protocol cores (consensus,
  wire session, control gate, spec sync, clock offset), no for connectors. Bench (M3
  Max, macOS, rustc 1.98.1): co-located 1.3/4.9/9.3 G samples/s at 1/4/8 producers, p99
  1.5 us at 80M samples/s; shared + locks 1.2/4.2/4.9, p99 3.2 us; handoff into shards
  p99 0.18-1 ms (macOS, no pinning; rerun on Linux). Two writers on one index: shared
  p99.9 115 us vs co-located 10 us.
- R8 BOUNDARY MAP (2026-10-04; report scratchpad/foundation-r8-boundaries.md, 992
  lines; ALL FORKS DONE). Proposed shape: layer 1 `types` (values), `spec` NEW
  (definitions, content-addressed tree, the one policy resolver), `codec`, `env` NEW
  (Clock, Fs, Rng traits); layer 2 leaves `transport`, `buffer`, then `time`, `blob`
  NEW (content-addressed store + peer fetch, for spec branches and upgrade binaries)
  which use transport, then `mesh` -> `home` -> `hub` (the narrow waist); layer 3
  `connector` (kind contract, actor, supervisor) + kinds incl. `connector-calc`,
  `connector-status` NEW; layer 4 `config`, `ops` NEW (operation table, generates
  cli/mcp/docs), `node`; `sim`. Top problems: layer 2 had no internal order
  (transport would call home upward); spec types had no layer-1 owner; no single
  encoding owner; standby replication + failover durability undesigned; status
  channels had no writer below hub; registering nodes in spec makes joins drift;
  secrets are a third kind of state; operation table had no crate; T1 breaks in iroh,
  Tokio, vendor libs; re-index splits history across homes. Open questions keystone
  first: Q1 layer-2 order + hub pulls from Transport::accept; Q2 split spec out of
  types; Q3 hub is the whole layer-3 window; Q4 encode once, validate at the home; Q5
  actor owns the loop, kinds implement Device hooks (Kind::open + start/read/write/
  stop); Q6 standby = complete reader with hold, reader positions travel as a channel;
  Q7 async failover, old home's tail returns as backfill; Q8 leases per group (names
  decide governance, placement decides home); Q9 index history in mesh, sealed by old
  home; Q10 placement covers connectors (standby connectors); Q11 nodes are runtime
  membership, status is a connector, owners co-locate companions; Q12 authenticate at
  hub, authorize at the owner; Q13 quality channel may share its data's index; Q14
  group run state via a command channel (A20 pattern; Synnax sy_task_cmd); Q15 synced
  device set = one connector when the driver acquires them as one unit (NI-DAQmx
  channel expansion, EtherCAT), else separate indexes, never two writers; Q16 secrets
  as own state sealed to every eligible node; Q17 `ops` crate; Q18 versions travel with
  lease renewals; Q19 `sim` ships in the binary (plan --simulate); Q20 wall time only
  from `time`; Q21 naming (`Block` for pooled bytes; `types` -> `value`?).
- BQ1 LOCKED (2026-10-04, "I think that's fine"): layer-2 internal order: transport,
  buffer (no core deps) -> time, blob (use transport) -> mesh (uses blob) -> home
  (mesh, buffer, time) -> hub (home, mesh, transport, time). Incoming connections are
  pulled: hub calls Transport::accept; transport never knows home or hub. `blob` NEW:
  one content-addressed store with peer fetch for spec branches and upgrade binaries.
- BQ2 LOCKED (2026-10-04): layer 1 splits: `types` = values (keys, data types, Frame,
  Series, selector matcher); `spec` = definitions (spec::Channel, spec::Node,
  spec::Connector, policy kinds, content-addressed tree, hashes, diffs) and the ONE
  policy resolver `spec::resolve` (no crate re-implements most-specific-wins). Each
  connector's config is an opaque document in spec; the kind decodes and checks it
  against its schema at plan time.
- BQ3 LOCKED (2026-10-04): hub is the whole layer-3 window: reader(), writer(), spec()
  read-only, watch(selector) for config changes, now() mesh time, block(len) pooled
  buffer. Not a pass-through (routing, fan-out across homes, live patterns,
  reconnection, authentication). Remote SDKs get the same surface over the network.
  User also asked to research Synnax telem and type architecture ("somewhat useful"):
  fork r9 launched -> scratchpad/foundation-r9-synnax-telem.md.
- HOME SPLIT (2026-10-04; user: "Might be worth isolating home into sub-crates for
  specific responsibilities. up to you though ... think carefully about single
  responsibility. like maybe there's a control package that is just about control
  filtering"; decided by me per "decide the best architecture"): `control` = the gate
  per index (authority, ties, writer leases, handoffs, values for the control channel);
  `delivery` = reader state per index (complete cursors + credits B3, latest slot B4,
  max age, holds and floors S10); `home` = the per-index write path (timestamp checks,
  seq A8, asks control, stores via buffer, hands to delivery). control and delivery are
  sans-I/O leaves of layer 2 (no core deps); layer-2 order becomes transport, buffer,
  control, delivery -> time, blob -> mesh -> home -> hub. Synnax precedent: control
  isolated in x/go/control + cesium/internal/control.
- ADAPTIVE COMPRESSION (user, 2026-10-04: "think carefully about intelligent
  compression algorithms. Random sensor data ... sometimes not worth compression.
  Maybe even setting compression policies. Not worth costing CPU time if we don't get
  compression yield."): proposed inside BQ4: codec samples each block and picks the
  scheme or raw (BtrBlocks sampling-based selection); raw is always a candidate;
  `[[compression]]` policy kind (S12) with mode auto | raw | max and a minimum saving;
  choice recorded per disk chunk and signaled on the wire only when it changes.
- SRP PASS LOCKED (2026-10-04, "I can be happy with the 6 splits"): six splits: `control` (from home: who holds control of an index),
  `delivery` (from home: each reader's position, credits, holds, what it gets next),
  `access` (from home and mesh: may this subject do this action on this name), `raft`
  (from mesh: replicated log, knows nothing about specs), `wire` (from codec: message
  format between two nodes, short key numbers, predicted seq), `block` (from types:
  pool of preallocated buffers, holds the unsafe code). Layer 1 = pure logic, no I/O
  ("decides"); layer 2 = drives disk, network, clock ("does"); BQ1 order unchanged.
  home keeps the write path; mesh keeps what is agreed, stored through raft; codec
  keeps compression of one series. Not split: hub, connector, config, spec.
- FORKS r9-r11 (2026-10-04): r9 Synnax telem/types (first fork attempt went off task
  and wrote nothing; relaunched as general-purpose); r10 adaptive compression + local
  benchmark (user: "Not worth costing CPU time if we don't get compression yield";
  feeds BQ4); r11 memory allocation, pooling, sharing, synchronization, queue sizing +
  micro-benchmarks (user: "extremely deep research ... the frame bitmask yielded
  massive performance improvements in go"; Synnax x/go/telem/frame.go mask uses
  bit.Mask128, frames up to 128 entries). Reports: scratchpad/foundation-r9..r11-*.md.
- BQ5 REVISED BY USER (2026-10-04): my proposal (connector actor owns the loop; kinds
  implement Kind::open + Device start/read/write/stop) REJECTED: "invert the
  dependency ... connectors can instead reach into a shared library of components ...
  inevitable edge cases ... where we initialize devices, how we pace their reads,
  software vs. hardware timing, socket configuration, maybe additional threads ...
  composition and a component based architecture as the primary pattern ... be
  extremely careful and wary of places where it may be wiser to INVERT the
  dependencies." Evidence: Synnax EtherCAT engine (driver/ethercat/engine/engine.h)
  runs its own RT cycle thread + seqlock + registrations behind Source::read; Synnax
  common::SampleClock (driver/common/sample_clock.h) already a component (software vs
  hardware timed); Telegraf ServiceInput beside Gather; Debezium ChangeEventQueue +
  own thread behind Kafka Connect poll; OTel optional scraperhelper; Petricek
  "Library patterns: why frameworks are evil" (frameworks cannot compose). New shape
  (proposed): a kind owns `run(ctx)`; supervisor only starts and cancels it; `ctx`
  hands out capabilities (hub sessions, status, run commands, secrets, cancel); hub
  and home enforce invariants (one writer per index, authority), not the loop;
  `connector` becomes a library of components; common cases use ready-made
  compositions built only from public components. Component catalog: fork r12.
  Durability: research reports copied to ~/.claude/projects/<proj>/foundation-research/
  (persistent); this file is the decision log.
- BQ5 LOCKED (2026-10-04, "I think that's a good start"): each kind owns
  `async fn run(&self, ctx: Context)`; supervisor only starts and cancels it; ctx hands
  out hub sessions, status, run commands, secrets, cancel; hub and home enforce the
  rules (one writer per index, authority, access); `connector` is a library of
  components plus ready-made compositions built only from public parts. Catalog and
  inversion audit pending (fork r12).
- TRADE STUDY GAP (2026-10-04; user: "How intense of a trade study have you done on
  this architecture?"): honest answer: deep per-area research (r1-r7), but the boundary
  questions are one fork's proposals with a few precedents each, replication/failover
  had no study (Q6 rested on two Kafka precedents), and the overall shape was never
  compared to alternatives. BQ6, BQ7, BQ8, BQ10 PAUSED until fork r13 (replication
  trade study: Raft per index, Kafka ISR, PI n-way buffering, chain replication, sync
  primary-backup, leaderless, per-placement durability; failure-mode matrix). Fork r14:
  whole-architecture alternatives (Zenoh as competitor and possible base, DDS, broker +
  edge agents, PI/Ignition/Canary historians, MQTT Sparkplug UNS, Synnax Core + Driver,
  Aeron etc.). Reports go to ~/.claude/projects/<proj>/foundation-research/.
- SEATING REQUIREMENT (user, 2026-10-04: "think carefully about how the standby and
  recovery architecture is seated. How deep does it have to reach into internals? can
  it be separated from the core with a clean boundary? should think about this piece
  for a lot of the separate components. if not that's ok and we need to do a trade."):
  for every feature, record internals touched, whether it is separable onto the core's
  public surface (yes, partly, no), the boundary, and the trade. Added to r13
  (replication: copy path vs takeover path seated separately; precedents Litestream,
  Postgres logical vs physical replication, MirrorMaker 2, Debezium) and r12 (feature
  seating table: standby, re-index, retention, time sync, status, access, control,
  calc, secrets, upgrades, discover, plan/apply).
- R9 SYNNAX TELEM RESULTS (2026-10-04; report foundation-research/foundation-r9-
  synnax-telem.md, 14 decisions D1-D14, 6 claims verified by running Go, 1 by Python).
  KEEP: i64 ns time with separate stamp, span, half-open range types and
  Stamp - Stamp = Span; one format/parse grammar (span parts, range elision, ns ISO);
  width per type (count = bytes / width); typed bytes in one little-endian buffer read
  as zero-copy typed views; same bytes in memory and on disk; parallel key and series
  arrays sharing buffers; stateful codec sending known facts once (A4 prior art);
  monotone per-index position (A8); validate and normalize bools once at ingest;
  buffer reuse, readable debug output, fuzzed codecs. FIX/DROP: four hand-written
  cross-language copies that drift; open string data types; density 0 meaning both
  unknown and variable; per-sample length prefixes (O(n)) and a stale Go length cache;
  JSON type; 64-bit ints losing precision in JSON; unchecked conversions (wrong-type
  reads give garbage, unaligned writes lost: verified); float time math (Python off
  24 ns, C++ f32 rate drifts 1.38 s/day); ambient clocks and local zones in values;
  weak time arithmetic (ts + ts); alignment as storage position (wraps, sorts wrong, 4
  fix commits: verified); per-series metadata and repeated keys in a frame; copies and
  allocations per hop; copy-on-write keyed on use_count; GL state in Series; panics on
  outside input; math kernels in types; node encoded in the channel key; same check in
  three layers; three bool formats. Proposed surface: time::{Stamp, Span, Range}, exact
  Rate, Size, Seq, channel::Key, node::Key, sample::{Primitive, Type, Native},
  block::{Pool, Unique, Block}, series::Series, frame::Frame.
- K1 LOCKED (2026-10-04, "Yes, I agree"): the boundary is a syntax-neutral
  `Document` (blocks, attributes, values, source positions on every value); each
  syntax is a front end that reads AND writes it (`config-hcl` first; TOML etc. later;
  YAML read-only until a sound format-preserving Rust editor exists); `config` checks
  only the Document (templates, names, units, plan) and never knows a syntax; a front
  end table keyed by file extension is built in `node`; HCL is the default (init,
  docs, agent guide); files hold data only (no loops, variables, modules); convert
  between syntaxes comes free; SDK code can produce a Document directly. Calculation
  expressions get their own small grammar, written as a string in every syntax
  (`expr = "(site_a.plc_7.pt_101 - 101.3) * 0.145"`), checked at plan; this replaces
  r3's "one language because HCL has expressions" (Q4). Never shrink the model to the
  weakest syntax (Viper lowercases keys, drops positions).
- LESSONS FOR THE RFC PRINCIPLES SECTION (user asked to write these down):
  (1) neutral model at boundaries, formats are adapters (feedback-neutral-model-at-
  boundary); (2) library of components, not framework; invert dependencies where it
  adds flexibility (feedback-invert-dependencies-library-not-framework); (3) a name
  that can get simpler means split the package (feedback-naming-tell-means-split);
  (4) dependency direction and structural simplification (feedback_design_
  dependency_direction); (5) every feature records how deep it reaches into internals
  (SEATING REQUIREMENT).
- VOCABULARY (user, 2026-10-04: "careful to use the term branch as branch could refer
  to git branch, and we eventually might want to introduce mesh branching kind of like
  neon has DB branching"): retire "branch" for the voter-governed part of the name
  tree; RESERVE "branch" for a possible future Neon-style mesh branching feature
  (copy-on-write fork of a mesh). Proposed replacement: "subtree" in prose, no new
  keyword (the voters policy's selector is the only config). Rejected: zone (ISA/IEC
  62443 zones and conduits mean security groupings in OT), domain (DDS domains, DNS,
  Windows), partition (Kafka, network partitions), region (cloud; Synnax control
  region), namespace (Kubernetes, Linux, our naming rule), realm (Kerberos, less
  plain), authority (taken by control authority S11), cell (ISA-95 level). Pending
  user OK. Also flagged for Q21: "block" means both HCL Document blocks and pooled
  buffers (block::Block).
- REGION LOCKED (2026-10-04; user: "I feel like region is really good. It tends to
  represent a distinct region like a site or factory. It is a commonly used and known
  term"): "region" replaces "branch" (and my "subtree" proposal) for the part of the
  name tree governed by one voter set. Every "branch" above in this log that means
  that concept now reads "region". Docs say "cloud region" for provider regions.
  Proposed next (pending): declare a region by its name prefix, `region "site_a" {
  voters = [...] }`, instead of a voters policy with a selector (a selector can express
  invalid regions like `**.cmd`; r4 requires a region to be exactly one subtree).
- REGION BLOCK + K2 SETTLED AS TUNABLE STARTING POINTS (2026-10-04; user: "Yeah fine
  I don't care that much about this, we can tune this syntax over time, this seems like
  a minor decision"): `region "site_a" { voters = [...] }` declares a region by prefix;
  regions nest like names; other policies keep selectors. K2: core knows only full
  names and regions; plan groups changes by region; directories mirroring regions are
  the default layout written by init/discover/export (plan warns on mismatch); full
  names everywhere; no imports; discover proposes diffs, never overwrites.
- C5 REDIRECTED BY USER (2026-10-04): r3's single-expression, no-loop calc language
  REJECTED as the ceiling: "building a powerful calculation engine could be fantastic.
  We already know how to build one of these languages, and building a simpler port in
  rust just wouldn't be that hard. Could make it possible to do incredibly valuable
  stuff like rule based filtering, waveform processing and FFTs ... 'calculations are a
  different integration' is fine because now we have a clear architectural boundary.
  Then we can dispatch an agent or an agentic swarm to build whatever calculation
  engine." Proposed (pending OK): lock only the boundary now; the engine (likely an
  Arc-style language in Rust) is a later, separate design. Boundary guarantees: (a)
  data only through hub sessions; (b) a plan-time check hook per kind (same seam every
  kind's config check uses) so name/unit errors still fail at plan; (c) resource
  isolation so heavy work (FFT) cannot starve acquisition (own threads and budget, or
  placement on another node); (d) determinism: time only from samples and ctx, for
  simulation; (e) outputs on the calc's own index (one writer per index; r3's "output
  uses the first input's index" would make a second writer).
- KINDS OWN THEIR CONFIG (user, 2026-10-04: "integrations/connectors or whatever we
  call them own their own config/spec parsing logic and then use an underlying
  substrate/componentry"): proposed (pending OK): each kind is a deep module owning
  parse, check, discover, and run, built on shared components; `config` parses files
  to Documents and handles core definitions (channels, regions, policies, types) but
  never knows a kind's fields; it hands each connector block to its kind via the
  table; the kind returns diagnostics with positions plus the channels it reads and
  writes (all plan needs: one writer per index, access, placement). Shared parsing
  components: Document reader with positions, name resolution against the spec, unit
  and duration parsing, diagnostics. Replaces r8's "config validates against each
  kind's JsonSchema". Synnax precedent: each driver integration parses its own config
  with shared x::json::Parser (x/cpp/json/json.h:105, field_err with paths, child()),
  e.g. driver/modbus/read_task.h:185. Calc kind is one instance: it parses and checks
  its own language.
- C5 + KINDS OWN THEIR CONFIG LOCKED (2026-10-04, "Yeah, I can agree"): each kind owns
  parse, check, discover, run on shared components; config never knows kind fields;
  kind returns diagnostics with positions + channels read and written. Calculations are
  a kind; the engine (powerful, Arc-style port in Rust; rule-based filtering, waveform
  processing, FFT) is a separate later design, buildable by an agent swarm. Locked
  guarantees: resource isolation (own threads with a budget, or placement elsewhere),
  determinism (time only from samples and ctx), outputs on the calc's own index.
- R6 TIME LOCKED (2026-10-04, "Yeah that's fine"): own mesh clock (pure core over our
  transport; half-RTT bound; fastest exchange per source; combine sources; widen bound
  with drift; slew only); read GPS, PPS, NIC hardware clock, OS daemon directly; follow
  the smallest measured bound, no fixed ranking; no PTP client in v1 (PTP sites work via
  ptp4l keeping the NIC clock, read as a source); device clock fitting (DAQmx, LabJack)
  is a connector-library component writing residual error to the index error channel.
- TIME SOURCES AS ADAPTERS (user, 2026-10-04: "Again time sync can use an abstracted,
  plugin based architecture right"): proposed (pending OK): neutral model
  `Measurement { at: local monotonic, offset, error }`; the time core is a pure
  estimator that never knows what a source is; each source is an adapter that runs its
  own loop and feeds measurements (mesh peers, GPS, PPS paired with NMEA, NIC hardware
  clock kept by ptp4l, OS daemon; later our own PTP client, Starlink dish, White
  Rabbit, timing cards); a source table built in node; each adapter probes for
  availability and privileges; the same estimator is a connector-library component for
  device clocks (other direction). R7 starting points stated to user (no objection
  yet): open62541, Sparkplug schema from spec text, pure-Rust Kafka, NI functions
  declared by hand, FIPS build later, thin generated MCP server. BQ13 presented, still
  open.
- TIME ADAPTERS + BQ13 LOCKED (2026-10-04, "Yes, I agree. Yeah, I think that's fine.
  I'll leave a lot of the quality decisions up to you"): time sources are adapters
  around Measurement; BQ13: a quality channel may be on its own index or share its
  data's index (same writer writes both; per-sample, RLE makes steady quality nearly
  free; shared quality on its own index stays for slow status). QUALITY DECISIONS ARE
  DELEGATED TO CLAUDE: decide them, record them, do not interview them. R7 starting
  points stand (no objection).
- GROUPS DROPPED + COMMANDABLE PARAMETERS (user, 2026-10-04: "why wouldn't these be
  separate connectors? I don't know if we need this group thing. a connector is kind
  of comparable to a task. maybe that connector can share certain common parameters
  like a connection spec ... reduce the amount of terminology" and "certain things
  represent runtime state and runtime state can be commanded. maybe the sample rate is
  a parameter that can be commanded as state, maybe not. things that can be commanded
  are maybe tunable or defined by the integration? then permissions of the connector
  config can decide whether that can be defined dynamically or not. then those should
  be set by channels"). REVISES C3 ("connector is the device, groups are the tasks"):
  a connector IS a task: one index for reading (one rate, one clock, one writer) or one
  reader for writing; no "group" term. Proposed (pending OK): shared endpoints need no
  new term: connectors naming the same endpoint (OPC UA URL, NI device) share one
  connection via a library component that owns the handle once per node (Synnax
  EtherCAT engine pattern; fixes the LabJack handle race); the kind's checker rejects
  combinations the hardware cannot do. Commandable parameters: the kind declares its
  parameters and which can change at runtime; connector config chooses which are
  commandable; each commandable parameter is a channel with an ack (A20); access and
  control authority decide who sets it; files give only the starting value for
  commandable parameters; `running` is a parameter every kind gets from the library
  (answers BQ14).
- R11 MEMORY/SYNC RESULTS (2026-10-04; report foundation-research/foundation-r11-
  memory-sync.md; bench outputs in foundation-research/mem-bench/; source in
  scratchpad/mem-bench/). Go bitmask: PR #1220 (2025-05-29) replaced slice-copying
  filters (4 allocs + 2 scans per subscriber per frame) with value Mask128; now 52-541
  ns, 0 allocs; remaining costs: lookup per entry per subscriber, broadcast-then-filter.
  M3 Max numbers: pool alloc+free 1.1 ns vs mimalloc 4.2, macOS malloc 9.2;
  cross-thread free of 4 KiB 6.2-7.2 ns with return-to-owner pool vs 60-130 malloc;
  shared lock-free pool 75-1,306 ns at 2-8 threads; one shared Arc 418-441 ns at 8
  threads; frame fan-out with one refcount per frame 18-26x cheaper than per series;
  SPSC handoff 49-77 ns, 0.6 ns/msg batched; wakeup 5.5-8 us, parking designs had ms
  p99.9 tails under load; Tokio mpsc between runtimes 38-89 ns; cached filter mask 4-5
  ns per frame at any size vs rebuild up to 78 us. BUG: spin-then-park handoff hung on
  Apple silicon (RCpc LDAPR load passed the sleep-flag store); fence on both sides;
  wake protocols need loom or shuttle. SHAPES TO DECIDE (M1-M5): M1 frames point to an
  interned key set of node-local u32 slots (revises S1); M2 readers get views (frame +
  mask), masks cached per key set, home routes by key set; M3 a series is a slice of
  one refcounted block per frame (revises S2); M4 each shard owns an injected pool,
  blocks return to the owner shard; M5 blocks use offsets, never pointers (shared
  memory with SDKs stays possible). Tunable: allocator (mimalloc; jemalloc out, fixed
  page size on Pi 5), queue sizes, spin windows, byte credits. Rerun on Linux x86-64
  (pinned, glibc) and Pi 4 (no LSE atomics, ~2.7 GB/s memcpy).
- R12 COMPONENTS + INVERSION RESULTS (2026-10-04; report foundation-research/
  foundation-r12-components-inversion.md; 14 decisions R12-1..R12-14; written BEFORE
  groups were dropped, so its `groups` composition and per-group run state need
  revising to connector = task + commandable parameters). Strain evidence: Synnax's one
  shared loop (pipeline::Acquisition -> Source::read) strained 9 ways: own threads
  (EtherCAT cycle, HTTP curl thread, scan thread), own retry layers (OPC UA pool
  breaker, restarts inside read for NI and LabJack), own pacing (LabJack vendor
  intervals, Arc's second timer), own sharing registries (LabJack handle cache,
  unmerged bus::Registry); Arc skipped ReadTask/WriteTask. Prior art: every
  framework-owned loop added an escape (Telegraf ServiceInput, Debezium
  ChangeEventQueue, ros2_control detached, rclcpp WaitSet); component-owned loops
  needed none (Vector, OTel, tower, embedded-hal, Linux "midlayer mistake"). Catalog:
  14 modules (cancel, pace, clock, retry, endpoint, link, drive, thread, queue, cycle,
  status, run, out, calc); 7 compositions in `compose` (groups, polled, clocked,
  pushed, cyclic, out, calc) as plain async functions with closures; one handler per
  error class (Retry at tick loop, Device at the unit, Config at supervisor); ctx =
  one run's scoped capabilities, kind's &self holds node-injected long-lived deps;
  sketches for Modbus TCP, NI DAQmx, EtherCAT. Inversions: I1 move status collection to
  layer 4 (r8 connector-status breaks C1); I2 upward flow only through values the upper
  crate pulls (no subscriber lists, no upward hooks); I3 time = pure estimator +
  injected time::Source (matches locked time adapters); I4 shard loop drives buffer (no
  private timer or thread); I5 endpoint ownership is a component, supervisor never
  overlaps two runs of one connector; I6 supervisor stays thin. Seating: on public
  surface: status, calculations, discover, plan, apply and upgrade orchestration; must
  stay inside home/buffer/mesh/time: control gate, access enforcement, re-index seal,
  standby takeover, retention trimming, time estimate; standby copy path can be its own
  component on two narrow internal calls: home::subscribe(Raw), buffer::append_at.
- CONNECTOR = TASK + COMMANDABLE PARAMETERS LOCKED (2026-10-04, "Yeah, I can agree
  with this reading"): no groups; connectors naming one endpoint share it via a
  library component; kinds declare parameters and which can change at runtime; config
  picks which are commandable; each is a channel with an ack; `running` from the
  library for every kind (BQ14 answered).
- M1 + M2 LOCKED (2026-10-04; user: "I think a slot table is fine. Focus deeply on
  performance. I think this research is good"): node-local u32 slots; interned key
  set per writer session; frames point to the key set id; readers get a view (frame +
  cached mask per key set and reader); home routes by key set. User asked whether
  UUIDs are written to the spec: answer: NOT in the files; files carry names only; the
  stored spec maps name -> key; apply assigns a UUIDv7 the first time a name appears
  (Kubernetes metadata.uid precedent); renames stay explicit (A4). UUIDs appear only in
  the stored spec, wire setup, and disk footers. Performance rulebook written:
  project_foundation_performance_rulebook.md (for agents, C9b2 performance agent,
  adversarial reviewers, and the RFC).
- MEMORY/PERF DELEGATED (user, 2026-10-04: "anything you figure out here that's a
  reasonable optimization is fine with me, as long as you're thinking deeply about
  memory"): decide memory and performance details myself, record them, add rules to the
  performance rulebook; do not interview them. LOCKED: M3 (one pool block per frame:
  header with key set id and sample counts, one descriptor per series {seq, offset,
  len}, series bytes back to back; series = slice; one refcount per frame; connector
  writes straight into hub::block(len)); M4 (per-shard injected pools, release returns
  to owner, no global pool); M5 (offsets, never pointers, in blocks). MY MEMORY BOUNDS:
  (1) hard per-node pool budget; pools reserve address space, commit pages lazily,
  purge after idle (Pi idle < 50 MB); (2) no reader pins unbounded memory: credits cap
  blocks held per reader; a reader that falls behind is served from disk, not memory
  (Kafka slow consumers); latest readers hold depth-1; (3) pool exhausted: live write
  records a gap (B5), backfill waits. R9 TYPES SETTLED BY ME: D2 one-byte bools (bit
  packing only as a codec choice); D3 pad raw series to element width, blocks 64-byte
  aligned; D4 `ends[n]` offsets; D5 `types` = byte layout only, meaning in spec; D6
  `time::Span` value, `duration` keyword; D7 JSON: RFC 3339 UTC with 9 fraction digits,
  span unit strings, keys as UUID strings, no JSON on the data path; D8 exact reduced-
  fraction Rate, u128 offset math; D9 atomic refcount, `Unique` writable, `Block`
  immutable after freeze, no copy-on-write; D10 panic on internal overflow, checked_*
  for outside values; D13 keep crate `types` (modules time, sample, series, frame,
  block, channel, node, quality, name), RENAME layer-2 `time` crate to `clock`
  (clock::Source, clock::Measurement); D14 checks once at the home (BQ4). D1 MODIFIED:
  per-entry types are interned once in the key set, not stored per series. D11
  REJECTED: M1's benchmark answered it (slots). D12 (SDK data path binds the Rust core)
  -> asked as a question.
- D12 REJECTED BY USER (2026-10-04: "I don't think that making copies of things in
  native language is that hard in an agentic world. Agents can easily keep
  implementations solid and in sync"): C7 stands: each SDK's data path is hand-written
  in its own language. Guard (mine, stated to user): the Rust codec is the reference
  oracle; one conformance suite of golden vectors generated from it runs in every
  SDK's CI; differential fuzzing (Rust encodes random frames, each SDK decodes,
  compare, and the reverse); the quality crew's drift agent watches it. Synnax's four
  codecs drifted because nothing tested them against each other. User added: "We should
  focus on writing the most performant and semantic implementation for each language":
  each SDK's data path is the fastest idiomatic implementation for that language (for
  example NumPy-native buffers in Python, typed arrays in TypeScript), never a port
  that mirrors Rust's structure.
- R13 REPLICATION RESULTS (2026-10-04; report foundation-research/foundation-r13-
  replication.md; Monte Carlo model in foundation-research/r13-model/; 10 decisions;
  uses "branch" = region). Healthy LAN pair with NVMe: every alternative loses < 3 ms
  at p99 on home failure; they differ only when the standby is behind or cut off.
  Recommended async shape with five changes: Q6 standby fed by the home's per-index log
  (stored bytes, reader positions, control handoffs, dedup marks) sent after the home's
  sync; copy path = own layer-2 component `replica` using only delivery (raw
  subscription), buffer (append_at), transport; takeover = home's crash-recovery path
  + one fence check; standby as a hub reader REJECTED (decode + re-encode per frame,
  loses dedup and position state; Litestream, Postgres logical replication,
  MirrorMaker 2 show the same limits). Q7 live writes never wait; home publishes
  "stored" and "replicated" watermarks; each writer chooses which confirms its frames;
  voters promote the standby when the home's lease lapses however far behind; old
  tail returns as dedup backfill; no automatic failback, no in-sync set, no
  per-placement durability setting. Q8 one lease per node in its own region's group;
  standby must be in the home node's region. Q10 one placement covers a connector and
  every index under its name; standby connector starts cold behind the same fence.
  Kafka ISR loses the same data then stays unavailable until an operator acts; Raft
  avoids loss but needs 3 copies and stops writes without quorum (breaks B5). Pi 4 SD
  card: send-after-sync costs ~1.6 s of data per crash; decision 6: send on receipt
  instead, or require an SSD.
- BQ6 LOCKED (2026-10-04, "That's ok"): async replication; `replica` component (own
  crate, layer 2) ships the home's per-index log (stored bytes + reader positions +
  control handoffs + dedup marks) using only delivery raw subscription, buffer
  append_at, transport; never touches the home's write path; takeover = home's crash
  recovery + one fence check, inside home. Reader positions travel in the log.
- BQ7 LOCKED (2026-10-04, "I think i'm ok with this. we can continue to make trades"):
  live writes never wait; home publishes "stored" and "replicated" marks per index;
  each writer picks which confirms its frames and resends unconfirmed frames after
  failover (seq dedup, B7); voters promote the standby on lease lapse however far
  behind; old home's tail returns as dedup backfill; failback manual; default sends to
  standby after the home's sync (SSD advised for critical homes; Pi SD ~1.6 s/crash).
- R14 ARCHITECTURE ALTERNATIVES RESULTS (2026-10-04; report foundation-research/
  foundation-r14-architecture-alternatives.md; 11 decisions). VERDICT: Foundation's
  shape is the right base; only it has all four: durable at source and held per
  reader, one control gate, time with an error bound, fleet plan/apply that lets a
  cut-off site change its own config. Per-sample designs reach 1e4-1e7 samples/s; only
  a series model reaches P1. DO NOT build on Zenoh (replaces ~1 crate of 20; breaks T1
  and C2 shards with own Tokio pools; second routing model); add a Zenoh connector
  later. M3 loopback, one run: Zenoh 1.10.1 TLS p99 44-59 us; Zenoh QUIC link 0.7
  Gbit/s vs 13-19 Gbit/s over TLS/TCP; idle RSS zenohd 9.5 MB, nats-server 17 MB,
  Synnax Core 74 MB. WEAKEST POINTS: (1) too many new parts at once (transport, Raft,
  spec tree, storage, protocol clients, runtime); (2) remote durable readers each pull
  from the edge home, so a weak link carries the same data N times; (3) userspace QUIC
  may miss P1; (4) failover (now r13); (5) lacks quarantine, death records, lossy
  reduction. TAKE: Zenoh multi-link with TLS over TCP as fast default + one upstream
  flow per remote home; DDS QoS vocabulary + plan-time incompatible-setting checks;
  brokers/NATS send once across the weak link, fan out near readers, fsync before
  durable ack; PI buffer queue per copy, deadband + swinging-door reduction, periodic
  offset correction; Ignition quarantine + failover contract stating loss; Sparkplug
  death certificates; Aeron replay by position, log + deterministic service; UMH Core
  as market proof. Main decisions: read copies near readers; measure QUIC vs TLS/TCP on
  Linux; a first phase with fewer new parts.
- TRANSPORT QUESTION (user, 2026-10-04: "what are we using for network protocols? we're
  using QUIC? how are we thinking about latency vs throughput i.e. stream
  reliability?"): answered: not locked; carriers become adapters under one session
  model (streams with priorities and reset, optional datagrams): QUIC (noq-proto), TLS
  over TCP, relay, diode; benchmark QUIC vs TLS/TCP on Linux early; reliability is per
  delivery mode, not per link; over TCP, latest mode keeps the kernel send buffer tiny
  (TCP_NOTSENT_LOWAT) so stale frames drop before queueing. BQ8 still open.
- FAILOVER DELEGATED (user, 2026-10-04: "I think we can play with this failover stuff
  over time. I think you can make a lot of the remaining decisions on this"). DECIDED
  BY ME, per r13: BQ8 failover follows the home's node: one lease per node from its
  own region's voters; the index's holder record lives in that region; standby in the
  home node's region; the name's region governs the definition. BQ10 one placement
  covers a connector and every index under its name; the standby connector starts
  cold behind the same fence as the home's writes. Failback is a planned move, never
  automatic. Remaining r13 decisions are parameters, tuned by the T1 simulation.
- TRANSPORT SHAPE LOCKED (2026-10-04, "Yes, I agree. It's a bit like the freighter
  transport in Synnax right"): one session model (prioritized, cancellable streams +
  optional datagrams); carriers are adapters under it: QUIC (noq-proto), TLS over TCP,
  relay (TLS on 443 via designated nodes), one-way diode carrier; benchmark QUIC vs
  TLS/TCP on Linux early; default per traffic class by measurement; reliability per
  delivery mode (commands reliable highest; latest drops stale via cancel or datagram;
  complete reliable ordered with credits; catch-up lowest); latest over TCP uses
  TCP_NOTSENT_LOWAT. Freighter parallel (freighter/go transport.go:27, unary.go:20,
  stream.go:20, middleware.go:62: one interface, grpc/http/websocket adapters,
  middleware): same idea one level lower; Foundation's model stays rich (datagrams,
  cancel, priority) and an adapter without a feature emulates it (TCP: datagram ->
  short cancellable stream), never shrinking the model.
- READ COPIES LOCKED (2026-10-04, "That's fine"): placement `copies = [node]` keeps a
  read copy of an index on a named node; a copy is a never-promoted standby fed by the
  same `replica` component; may sit in another region; remote readers read and hold at
  the copy, so a weak link carries each sample once; hub merges latest subscriptions
  for one remote home into one upstream flow. User asked "A remote reader pulls from
  the home ... aren't we using a push based architecture?": answered: push with
  reader-granted credits (B2 subscribe once, home pushes, B3 credits pace it; a slow
  complete reader that runs out of credits catches up from disk); "pull" was loose
  wording for "the reader opens the subscription and its position lives at the home".
- BQ12 GAP FOUND (2026-10-04): r8's "authenticate at hub, authorize at owner" lets a
  forwarding node impersonate any subject (a remote SDK on node A writing to home H:
  H sees node A). Fork r15 launched (identity across forwarding: direct-to-owner vs
  subject-signed delegation tokens vs node trust; Tailscale, NATS JWT/nkeys, Kafka,
  CockroachDB, Zenoh, DDS Security, Biscuit, SPIFFE, Kerberos). BQ12 waits for it.
- R10 COMPRESSION RESULTS + BQ4 SETTLED BY ME UNDER THE MEMORY/PERF DELEGATION
  (2026-10-04; report foundation-research/foundation-r10-compression.md; tables in
  foundation-research/compress-bench/out.md; M3 Max, 18 synthetic datasets, bit-exact
  decode checks; Pi 4 rerun owed, report section 3.8). Numbers: one stats pass per
  1024-value vector gives exact sizes for every light codec (FFOR, delta, RLE, stride,
  float delta) at 0.10-0.20 ns/value (8-32-bit) and 0.45-0.48 (64-bit); detecting
  incompressible white noise costs 0.11 ns/value over a copy. 16-bit ADC: 3.96x at 1
  LSB noise, 2.00x at 16, 1.36x at 256, 1.00x full-range white noise; 24-bit 3.15-4.65x.
  Encode 0.15-0.37 ns/value ints, 1.3-1.5 timestamps, 1.65-1.76 f64. Per-vector choice
  beats one codec per series by 42-70% on mixed data. Weak: calibrated f64 only
  1.61-1.80x (4.4-5.0 bytes/sample, over P1's 4); ALP and ALP_rd fail; new `fdelta`
  within 2-17% of pco size at < 1/100 CPU. zstd -1: 0.99-1.33x on noisy data at 7.9
  ns/value; pco 2-23% smaller but 12-118 ns/value; Btrfs-style byte entropy mispredicts
  3-300x. Minimum saving 1/8 lost no yield; 1/4 hurt. DECIDED: select per vector from
  exact sizes; raw always a candidate; fixed 1/8 minimum saving; no sampling, no
  hysteresis (except ALP top-5 exponent pairs refreshed every ~100 vectors); 1-byte
  codec tag per vector (vectors stay independent); add fdelta, drop ALP_rd, natural-
  order delta, drop zstd; policy `compression { select, mode = auto | raw | max }`,
  auto default, max adds pco per vector; raw counts + calc scaling stays the default
  way to meet P1 (A17). BQ4 (encode once at entry: home for local, writer's hub for
  remote; decode once at the reader; each series decodes by itself) LOCKED; home
  validation: data series get header + structural checks only (lengths, widths, tags;
  no decode); index series are decoded (S6 strictly increasing, and the home needs
  timestamps); the validator is the top fuzz target.
- BQ9 LOCKED (2026-10-04, "I can agree with this"): re-index by changing `index` in
  the file (plan shows it); old home seals at its last accepted sample and records the
  seal with its region's voters; history "index A until T, index B from T" is runtime
  state in mesh; spec keeps only the current index; readers' hub joins the spans; old
  home down -> voters seal at its lease end; no data moves; a connector rate change is
  not a re-index; Iceberg partition evolution precedent.
- BQ11a LOCKED (2026-10-04, "Ok, that's fine. This works with IAC tools?"; asked after
  a /restate for a much simpler explanation): joining is an operation, not a file
  edit: admin creates a join ticket, node joins with it, voters record membership,
  joins logged on mesh.changes; files name nodes only where they matter (voters,
  placement). IaC answer: ticket is an operation (CLI, MCP, API), so a Terraform
  provider resource creates it and passes it to the VM's startup script (Tailscale
  `tailscale_tailnet_key` precedent); ticket options: single-use or reusable
  (autoscaling), expiry, scoped to a region and name prefix; ephemeral nodes are
  removed after being offline a set time; destroy calls node removal. Tickets are
  secrets: never in files (K4).
- BQ11b LOCKED (2026-10-04; user: "Yeah that's fine or they become part of the
  integration. but they can also be used for decision making and load rebalancing so
  IDK. As long as we have a clean respect of architectural models"): `node` reads each
  crate's values (pull only) and writes status channels through hub. Rule: the crate's
  value is the source of truth; the channel is a published copy for people, agents, and
  outside tools. Core decisions (failover, fencing) use internal values, never read
  back from channels. Rebalancing is an outside controller (agent or tool) that reads
  status channels and acts through plan/apply operations, like Kubernetes controllers
  acting through the API; it never reaches into the core.
- SETTLED BY ME (2026-10-04, stated to user as one-liners): BQ15 resolved by connector
  = task: a set of devices the driver acquires as one unit (NI-DAQmx channel expansion,
  EtherCAT) is one connector; otherwise separate connectors and indexes, never two
  writers. BQ17 `ops` crate holds the operation table and generates CLI, MCP, docs.
  BQ18 desired version in the spec; nodes report versions; a rollout lock upgrades one
  node at a time; finalize when all report (C9d). BQ19 `sim` ships in the binary behind
  a feature on in product builds (plan --simulate); measure size against P1. BQ20 wall
  time only from `clock`; clippy disallowed-methods + architecture agent. BQ21 names
  stay tunable (types modules per r9; `clock` crate; "block" collision noted). r4
  reconciliation: definition references (index, quality, error, control) stay inside
  one region; placement (home, standby, copies) is free per BQ8 and read copies.
- BQ16 LOCKED (2026-10-04, "Yeah that's fine"): secret value encrypted per eligible
  node (placement's nodes, standbys included; SOPS multi-recipient precedent); voters
  store ciphertexts outside the spec; only secret set/delete change them; on key
  rotation or new standby, a node that can decrypt re-encrypts for nodes placement
  names (closes the rotation gap); discover takes credentials for one call only.
- SECRET STORES AS ADAPTERS (user, 2026-10-04: "could also have a secret service
  that is a separate storage plugin/adapter"): agreed: neutral model = resolve a named
  secret to a value on the node that runs the connector; `ctx.secret(name)` is the only
  path for kinds, which never know the store; adapters: built-in sealed store (BQ16,
  default, works offline), node environment variable or file (K4), HashiCorp Vault,
  AWS Secrets Manager, Azure Key Vault, GCP Secret Manager, Kubernetes Secrets; a
  policy (S12 style) picks the store per name; external adapters authenticate with the
  node's own key (e.g. Vault cert auth); trade: a cloud store is unreachable from a
  cut-off site, so external adapters may cache values sealed to the node key, which
  delays revocation. R4 SETTLED BY ME: own sans-I/O Raft (etcd model; etcd scenarios
  + TLA+ as oracles), no gossip, prolly-tree spec storage (~4 KiB chunks, BLAKE3),
  voters on one LAN. K5 REVISION (region changes its own voters; parent creates or
  removes regions or forces takeover with an epoch bump) presented, awaiting answer.
- SECRET ADAPTERS + K5 REVISION LOCKED (2026-10-04, "That's fine" / "on K5 that's
  fine"): K5 now: a region changes its own voters; the parent only creates or removes a
  region, or forces a takeover (admin on parent, `--force`, epoch bump; nodes reject
  old-epoch commits); DNS zone delegation precedent; parent cannot veto.
- QUALITY DECISIONS BY ME (delegated): DEATH RECORDS: when a writer session ends
  without closing (lease lapse, connection loss), the home writes a "source lost"
  quality sample to each affected index's quality channel(s) as a companion write
  (Sparkplug NDEATH/DDEATH precedent); a clean close writes nothing. S7 OPTIONAL GAP
  CLOSED per r2: presence is per frame; a frame may carry a subset of its index's
  channels (presence mask over the writer's key set in the frame header); struct views
  are built from one frame, never from per-field latest values; disk chunks record
  covered seq ranges.
- REDUCTION LOCKED (2026-10-04; user: "Ok, just make sure that we aren't having
  architectural disagreements about how and where we define data structures and i'm ok
  with this"): rule: a policy never creates channels; anything that creates a channel
  is an explicit definition. Deadband = `reduction { select, deadband = "0.1 degC" }`
  policy (unit-checked; most specific wins; read by connectors through a library
  component; OPC UA and Sparkplug pass it to the device); frames carry only channels
  that moved. Swinging door = a calc with its own index. Raw + reduced side by side
  via retention policies.
- QUARANTINE DECIDED BY ME (component pattern): an out connector that gets a permanent
  rejection for a frame moves it to that connector's quarantine (a hold on the
  original data plus a record of the error) and advances; ops list, retry, drop;
  quarantine size is a status channel. Ignition precedent. A library component.
- CONSISTENCY AUDIT: fork launched to write foundation-research/foundation-decisions.md
  (current state by topic, "where things are defined" table, contradictions with
  resolutions, final crate map, open items). User asked "How many questions left?
  Seems like a lot of these can start to be resolved by actually going through and
  starting to implement critical things": answer: one shape question left (BQ12
  identity, waits on r15); the rest are parameters for experiments.
- OWN REPO + MULTI-SESSION FACTORY (user, 2026-10-04: "I can boot up for example 5
  claude sessions and we can all start doing comms with them. they can work together,
  open pseudo-PRs and we can continue working over the course of hours or days. That's
  a fine architecture. We're also going to work on a completely independent repo from
  the synnax mono repo for now ... synnaxlabs/foundation repo I created ... keep
  lessons learned, but we're building a new, second product"). REVISES C9a (no
  `foundation/` dir in the monorepo; own repo github.com/synnaxlabs/foundation,
  private, created 2026-10-05 UTC, empty; cloned to ~/Desktop/synnaxlabs/foundation)
  and C9d (tags vX.Y.Z in the own repo). Sessions launched in that repo get a
  DIFFERENT Claude memory dir, so the repo itself must carry everything: CLAUDE.md
  (lessons, prose and comment rules, naming, architecture principles, performance
  rulebook, git rules incl. no Claude co-author), docs/decisions.md (consolidated
  record), docs/research/ (r1-r15), docs/coordination.md (roles, comms, pseudo-PR and
  interface-change protocol), docs/rfc/. This session = coordinator/architect (owns
  interfaces, decision record, merge queue with the user). Proposed builder roles: (1)
  data path core: types, block, codec, wire; (2) pure logic: control, delivery,
  access, spec; (3) consensus: raft, mesh, blob; (4) network and time: transport
  (QUIC vs TLS/TCP benchmark first), clock; (5) storage and write path: buffer, home,
  replica. Later waves: hub, connector library + kinds, config + config-hcl, ops,
  node, sim, SDKs. Builders cross-review each other's pseudo-PRs adversarially.
- R15 IDENTITY RESULTS (2026-10-04; report foundation-research/foundation-r15-identity.md,
  copied to the foundation repo docs/research/r15-identity.md): recommend 2b: remote
  subjects sign a hello (subject, key, gateway node key, connection key, 10 min expiry
  renewed at half) and sign each session open and control-plane request over exact
  bytes; gateway forwards; owner verifies against subject keys in spec + peer node +
  expiry vs mesh time, then C8 + S11. Hosted subjects: a node acts only as itself or
  connectors placed on it (SPIRE parent ID). Internal traffic: node keys at transport,
  authorized by role (voters, placement, time policy), never access policies. Trade:
  compromised gateway can read/alter its own clients' traffic and reuse their open
  sessions until hello expiry; cannot impersonate others, open unrequested sessions,
  raise authority, or apply config. Ed25519 verify 22.5-25 us M3 Max, ~267 us Pi 4;
  per connection and per open, never per frame. Its decisions 2-10 DECIDED BY ME
  (delegated quality): sign each open (2b); subject key list, OpenSSH ssh-ed25519
  format, ssh-agent signing, issuer certs/SSO later; apply signs plan hash + base spec
  version; every node checks every mesh.changes record (Tailnet Lock idea); internal
  traffic by role; audit records subject + forwarding node (S11 control value adds
  `via`); MCP runs as a local stdio process beside the agent; caller seals secret
  values to target node keys (settles K4/BQ16 sealing); no end-to-end frame integrity
  in v1. Decision 1 (two proofs + the trade) presented to user as BQ12.
- MODELS LOCKED (user, 2026-10-04: "Should we use fable for any of the work?" then
  "That split is fine"): Fable 5.1 (documented for demanding reasoning and long-horizon
  agentic work; 2.5x Opus 5.5 API price; Max limit cost undocumented) for the consensus
  builder (raft, mesh), the memory builder (block pools, lock-free rings, wake
  protocols), and reviewers of those; Opus 5.5 for coordinator, other builders, other
  reviewers; Sonnet 5.5 for code-quality and drift crew. Review after a day of limit use.
  Written into the foundation repo: docs/coordination.md "Models", review skill,
  agent frontmatter. User added (2026-10-04): "I also want to be able to set clear /
  goal commands for the agents for them to work continually. can also use sonnet when
  its relevant": Sonnet also for mechanical work (format, renames, regenerated code,
  CI-only fixes) via subagents; builders run `/goal <condition>` (documented: works
  turn by turn until a small model judges the condition met from the transcript;
  `/goal clear`), coordinator runs `/loop /coordinate` (self-paced); idle sessions wake
  on SendMessage; `--permission-mode auto` for long runs. Written into
  docs/coordination.md "Running continuously".
- Next: audit results -> resolve contradictions -> bootstrap repo (CLAUDE.md, docs,
  workspace skeleton with compiled interfaces, env + sim, layer check) -> kickoff
  prompts for the builder sessions -> BQ12 when r15 lands -> RFC in docs/rfc/. (region changes its
  own voters, own sans-I/O Raft, no gossip, prolly tree), r14 features (quarantine,
  death records, deadband/swinging-door reduction), revised component catalog,
  first-phase scope., folding in fork results (r1 thread
  model, r2 storage, r3 languages, r4 consensus incl. K5 revision, r5 transport, r6
  time, r7 dependencies incl. compiled-in C re-check, done), then r9 telem into
  `types`. Next up: BQ4 encode once, validate at the home.

**Why:** the user wants a design they can build with agents; locked answers must not
be re-interviewed. **How to apply:** read this before resuming the Foundation design;
append each decision as it locks.
