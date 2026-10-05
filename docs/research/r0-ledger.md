# Foundation research ledger (2026-10-04)

Evidence base for the Foundation RFC. Five research tracks. UNVERIFIED marks claims
without a primary source.

## 1. Market and prior art

- Incumbents are GUI-first or Windows-bound: Kepware Server (Windows GUI; Kepware Edge
  is headless via Config API), Cogent DataHub (Windows only), Litmus Edge (UI + Edge
  Manager templates, no text config in Git), HighByte (UI-first, Git integration added
  v4.2 July 2025). Ignition 8.3 moved config to JSON files, but Git friction remains
  (signatures in resource.json, one rename touches thousands of files).
- Text-config tools configure one node, not a fleet: Telegraf (TOML), Vector (YAML),
  Redpanda Connect (YAML), UMH Core (one container + config.yaml, Apache 2.0, UNS over
  embedded Redpanda; closest developer competitor), Cybus Connectware (YAML
  commissioning files).
- Store-and-forward elsewhere: HiveMQ Edge paid; Telegraf disk buffer experimental and
  uncapped; Azure IoT Operations disk buffer ephemeral; Ignition S&F only between
  Ignition gateways; Litmus per-connector checkbox.
- Pricing pain: Kepware per-instance and per-plugin fees, Neuron free to 30 tags,
  HighByte from $17,500/site/yr, Azure $175/node/mo + $0.14/asset/mo, AWS SiteWise Edge
  $200/gateway/mo. Kepware sold to TPG 2026-03-16.
- Developer-first winners (Tailscale, NATS, Zenoh, Redpanda, Temporal, Vector, Viam):
  single binary that runs on a laptop with no config; free tier useful in production;
  open data plane with paid control plane or cloud; text config plus CLI, API, and
  Terraform; runnable docs. License reversals cost trust (Synadia BSL attempt reversed,
  Redpanda Connect enterprise connectors, Foxglove closed 2.0).
- Top user complaints: per-tag licensing; GUI/Windows config that cannot be versioned;
  fragile reconnects and silent data loss (Telegraf OPC UA BadSessionIdInvalid loops);
  Sparkplug rebirth storms and dialect differences; OPC UA certificate pain; K8s and 16
  GB footprints.
- Market gaps: fleet-wide config as code with plan/diff/apply; free, bounded,
  observable store-and-forward with end-to-end acks; time quality as a feature;
  conformance-grade Sparkplug, Ignition, and version-aware Influx writers; agent-safe
  deploy/diff/rollback (incumbent MCP servers expose data only).

## 2. Integration hazards

- InfluxDB: three write APIs (v1, v2, v3 `write_lp`). v3 defaults `accept_partial=true`
  and `no_sync` acks before WAL persist; parse rejects per line. v3 Core: 5 databases,
  500 columns per table, tag/field name conflict. Same series+timestamp overwrites, so
  retries are idempotent.
- Kafka: EOS covers only data inside Kafka. Ordering per partition; key choice sets
  ordering and hotspots. Schema Registry framing (magic 0, 4-byte ID, protobuf index
  varints). SASL/SCRAM, mTLS, OAuth, MSK IAM.
- Sparkplug B 3.0: NBIRTH/NDEATH QoS 0; NDEATH is the Will with bdSeq; seq 0-255 wraps;
  NDATA/DDATA MUST be QoS 0; Primary Host STATE retained QoS 1; reorder timeout then
  Rebirth NCMD; is_historical for backfill.
- Ignition: Sparkplug into MQTT Engine (customer buys Engine, $2,150), Module SDK (Java,
  signing granted by IA staff, API breaks across versions), OPC UA server (free, no
  history), 8.3 Event Streams Kafka source. 8.3 Core Historian is QuestDB.
- Grafana: a vendor's backend plugin is "Commercial" and needs a paid subscription to
  be signed for distribution; Grafana Live accepts line protocol at
  `POST /api/live/push/:streamId`, stores nothing, defaults to 100 WS connections.

## 3. Mesh, security, time sync, IaC

- Per-hop reliability is not end-to-end: Zenoh routers in Drop mode lose samples; NATS
  core drops slow consumers; Sparkplug data is QoS 0. Zenoh AdvancedPublisher adds
  origin seq, publisher cache, heartbeat miss detection (opt-in). NATS JetStream dedups
  on Nats-Msg-Id over 2 min; mirrors keep origin seq. Jepsen (Dec 2025): JetStream
  2.12.1 lost ~14% of acked writes on power loss (fsync every 2 min default).
- DDS: QoS mismatch silently blocks communication; multicast O(N^2) discovery pushed
  ROS 2 to rmw_zenoh.
- iroh 1.0 (2026-06-15), 1.3.0 (2026-09-28): dial by Ed25519 key, QUIC + TLS 1.3, hole
  punching (~9 in 10 direct), relay fallback over WebSocket, self-hostable relays,
  datagrams, multipath. Transport only: ordering, durability, dedup are ours.
- Satellite: GEO RTT ~600 ms; PEPs cannot split QUIC; BBR misjudges bandwidth. DTN
  BPv7 (RFC 9171) custody transfer as prior art (UNVERIFIED).
- Tailscale: coordination server + P2P WireGuard + DERP over 443; tagged pre-approved
  auth keys; HuJSON policy with inline tests, GitHub Action test on PR / apply on
  merge; Terraform provider. Headscale: self-hosted, common at air-gapped sites.
  Nebula: CA-signed certs, one-year CA, manual rotation pain. Teleport Machine ID:
  20-minute certs, Bound Keypair enrollment (no secret crosses systems). SPIRE: TPM
  DevID or x509pop for bare metal.
- Industrial: IEC 62443 zones and conduits; Purdue L3.5 DMZ forbids direct IT-OT
  traffic; data diodes use one-way UDP with FEC; outbound-only patterns (NATS leaf
  nodes, argocd-agent).
- Time: linuxptp ptp4l/phc2sys; gPTP needs time-aware bridges; NI cDAQ-9185/9189 sync
  to under 1 µs over 802.1AS. chrony: tens of µs on LAN, sub-µs with HW stamps.
  ntpd-rs in production at Let's Encrypt, Ubuntu plans it default in 27.04; statime
  merging into ntpd-rs, comparable to linuxptp per NMi. Windows W32Time best tier 1 ms
  with strict conditions; Windows PTP client is software-timestamped, NTP-class. DAQmx
  sets t0 once from PC clock then adds n x dt (~4 s/day drift at 50 ppm). LabJack T7
  drift up to 10.4 s/day vs host; LabJack advises re-anchoring. Unprivileged adjtimex
  read gives offset, max error, sync state; changes need CAP_SYS_TIME. CockroachDB
  stops a node at 80% of max-offset. AWS ClockBound publishes an error bound.
- IaC: OpenGitOps (declarative, versioned, pulled, reconciled). Kubernetes spec/status,
  conditions with observedGeneration, server-side apply field ownership, dry-run and
  diff. Terraform plan -json and -detailed-exitcode for drift. clig.dev: --json on
  stdout, --dry-run, --no-input, idempotent, crash-only. MCP tool hints
  (readOnlyHint, destructiveHint, idempotentHint) (UNVERIFIED against spec text).

## 4. Rust ecosystem ledger (verified 2026-10-04)

- OPC UA: async-opcua 0.19.0 (MPL-2.0, tokio, client+server, subscriptions, history
  services, store is ours). Crypto risk: RustCrypto `rsa` Marvin leak
  RUSTSEC-2023-0071 open, no ECC policies, not certified. Fallback: open62541 0.12.0
  wrapper over C 1.5.x (has ECC). Verdict: use and move crypto to aws-lc-rs.
- Modbus: tokio-modbus 0.17.0 (pure). Serial: serialport 4.10.1 (disable libudev for
  musl) or serial2.
- MQTT: rumqttc 0.25.1 (slow upstream; fork rumqttc-next 0.34.0), ntex-mqtt 9.0.0.
  Broker: rmqtt 0.24.0. Sparkplug: srad 0.5.0 (single author; vendor).
- Kafka: only rdkafka 0.39.0 (librdkafka C) has groups + transactions. rskafka pure but
  produce-only. Behind a feature flag.
- NI DAQmx: no runtime-loading binding exists; write our own over libloading 0.9.0 +
  bindgen dynamic loading. DAQmx Linux: RHEL 9.6/10.0, openSUSE 15.6/16.0, Ubuntu
  22.04/24.04, x86_64 only; no macOS, no ARM.
- LabJack: LJM wrappers dead and link at build time; write own dlopen wrapper, or own
  Modbus TCP (502) + spontaneous stream (702) client for Ethernet devices.
- EtherCAT: ethercrab 0.7.1. CAN: socketcan 4.0.0 (Linux only).
- InfluxDB: write own line protocol; influxdb3-client 0.3.0 too young. Arrow 60.0.0
  behind a feature flag.
- QUIC: quinn 0.11.12 (datagrams). TLS: rustls 0.23 + aws-lc-rs (FIPS 140-3 certs on
  AWS-LC 3, aws-lc-rs <1.18; iroh defaults to ring, so force one provider).
- Time: ntpd 1.9.0 (no Windows); statime 0.4.0 (no release in 19 months, Linux daemon
  only); timestamped-socket 0.3.0 (no Windows).
- Storage: redb 4.3.0, fjall 3.1.12; no maintained WAL crate; write our own WAL.
- Serialization: prost 0.14.4, buffa 0.9.2; bincode archived (RUSTSEC-2025-0141).
- Testing: turmoil 0.7.2, shuttle 0.9.5, loom 0.7.2, proptest, cargo-fuzz, kani 0.68,
  cargo-mutants 27.1.0. madsim slowing.
- Build: cargo-zigbuild 0.23.4, cargo-xwin 0.23.1. Service: windows-service 0.8.1,
  sd-notify 0.5.0. Update: self_update 1.3.0.
- Riskiest gaps: OPC UA crypto, DAQmx dlopen layer, LabJack, Kafka transactions via C,
  Windows time sync, FIPS provider pinning, single-maintainer deps, the WAL.

## 5. Synnax lessons (file:line in the research report)

- KEEP: explicit per-sample timestamps (fixed-rate channels deleted, RFC 0008);
  hardened reconnect policy (client/ts/src/framer/hardened.ts:72-203); hardware sample
  clock with capped PID correction (driver/common/sample_clock.h:94-241); deadline-
  anchored timers (PR #2788); save vs deploy with config hash drift (RFC 0056);
  migration conductor; bundles as diffable directories (RFC 0039, 0052); protocol
  simulators and repro-first fixes; status dedupe; read-only doctor (RFC 0060).
- CHANGE: validate timestamps at ingest (1970 data incident); persistent per-stream
  sequence numbers, not storage position (alignment regression); one clock read per
  cycle, atomic grouped writes (RFC 0058 arc cycle timestamp); binary framing with one
  codec; heartbeat on every stream, deadline on every request (half-open socket hang,
  v0.57); acquisition independent of the upstream link (driver/task/manager.cpp:
  176-194); one error taxonomy enforced per connector (OPC UA TEMPORARY unused); one
  device actor owns each handle (LJM handle race, libmodbus not thread-safe); typed
  device properties (Modbus swap fields never read); address by protocol address, not
  name (EtherCAT SY-5092); chunk Modbus reads, use OPC UA subscriptions and source
  timestamps; own clock sync instead of warn-only skew; homegrown gossip never shipped
  multi-node (Aspen); never encode ownership in IDs (12-bit node key in channel key);
  fault harness before features; fsync, checksums, repair on open (Cesium never
  syncs); one integer version per resource (three versioning rewrites, RFC 0033, 0048,
  0053); typed config from day one (NI config in 3 languages); generate help/docs from
  the registry; per-test in-process node on a random port; fault proxy and scale tests
  in the standard suite; rustls only, vendor SDKs loaded at runtime; real target
  triples tested on target; no stdout handshakes; licensing never stops a running node;
  "running" requires recent data; desired state reconciled at boot.

## 6. Agentic software factory

- Every strong result had an oracle the agents could not change: Carlini C compiler
  (GCC torture suite; 16 agents, ~2,000 sessions, ~$20k API, 100k lines Rust, 2
  weeks); Bun Zig-to-Rust (TypeScript suite, 1.39M expects, 0 skipped; 64 Claudes peak,
  implementer + 2 adversarial diff-only reviewers + fixer; ~$165k API; 11 days);
  Ladybird (test262, byte-identical AST, lockstep C++/Rust; zero regressions);
  StrongDM (holdout scenarios outside the repo, Digital Twin Universe). Weakest oracle,
  weakest result: Cursor browser (88% CI failure claims).
- Coordination: flat peers with locks collapse to the throughput of 2-3 agents
  (Cursor); recursive planners + context-free workers + judge works; agents ran git
  stash/reset on each other (Bun) so ban them and isolate worktrees; Carlini used lock
  files in current_tasks/ and a sampled --fast suite.
- Drift: OpenAI harness engineering used a ~100-line AGENTS.md map, a fixed layer order
  enforced by custom linters with fix-it messages, and background cleanup agents.
- Verification: DST (FoundationDB, TigerBeetle VOPR, S2 mad-turmoil found 17 bugs with
  a rerun-and-diff meta-test); agentic PBT (Anthropic, 86% valid top reports); OPC UA
  CTT is members-only; Modbus conformance via members or labs; Paho test broker for
  MQTT; DAQmx simulated devices and LJM demo mode are shallow, so HIL rigs are needed;
  LLM TLA+ matches code only ~41-46% without trace validation; Kani; cargo-mutants
  --in-diff.
- Claude Code: claude -p, workflows (16 concurrent default), worktrees, hooks
  (PreToolUse deny, TaskCompleted exit 2), routines (individual account, draw
  subscription usage), GitHub Action (org secrets should use API key).
- Plan terms (verified 2026-10-04 against anthropic.com/legal/consumer-terms and
  code.claude.com/docs/en/legal-and-compliance): Consumer Terms prohibit access
  "through automated or non-human means, whether through a bot, script, or otherwise"
  except via an API key or where explicitly permitted; no credential sharing or making
  an account available to others. Claude Code legal: "Advertised usage limits for Pro
  and Max plans assume ordinary, individual usage of Claude Code and the Agent SDK";
  developers building products or services "should use API key authentication".
  API per MTok (input/output): Opus 5.5 $4/$20, Sonnet 5.5 $2/$10, Fable 5.1 $10/$50;
  batch 50% off. Team Premium $100-125/seat/mo.

## 7. Monorepo integration (read-only survey, 2026-10-04)

- No root Cargo workspace or rust-toolchain file. Only Rust today: console/src-tauri
  (edition 2024, rust-version 1.98) and the vendored wasmtime submodule. No clippy,
  rustfmt, cargo-deny, or taplo config tracked.
- CI: ci.yaml is the single entry; a `changes` job runs dorny/paths-filter over
  .github/filters.yaml; each filter key is repeated as an output; jobs gate on
  `needs.changes.outputs.X`; the `OK` job must list every job in `needs` to gate
  merges. Always-on jobs: copyright, format (prettier walks the repo, incl. TOML and
  YAML), versions. Rulesets require only `Review gate` and `OK`; merge queue; squash
  only.
- Release (RFC 0058): dispatch-only, tag is the version, `product/v` tag prefix,
  hotfix via release/<product>-X.Y. Off-train precedent: release.desktop.yaml and the
  desktop special cases in .github/scripts/resolve_version.sh (:29-35, :46-91).
  release/foundation-X.Y already matches release.json protection.
- Review gate: check_review.sh reads tier labels, needs Greptile 5/5, review/bot needs
  no human approval; runs from the base branch. No CODEOWNERS. A per-directory rule
  needs CODEOWNERS + ruleset flag, or a path check in check_review.sh (+ tests).
- Agent guardrails: .claude/* is gitignored except skills/. No tracked settings.json,
  hooks, or agents. Protected oracle paths need a tracked .claude/settings.json deny
  rule plus a PreToolUse hook matching Edit|Write|MultiEdit|NotebookEdit|Bash (exit
  2), with .gitignore negations.
- Hazards: `bazel build //...` and `buildifier -r .` walk the tree (add foundation to
  .bazelignore); .gitignore rules hide `**/bin/**`, `**/data`, `gen/`, `build/`,
  `dist/`, `**/ignore*` (avoid those dir names in crates); copyright checker covers
  .rs and .toml with the 8-line BSL header; rustfmt needs max_width = 88.

## 8. Type system prior art (2026-10-04)

- OPC UA: EnumStrings/EnumValues, OptionSet (value + validBits), StructureType
  (Structure, WithOptionalFields, Union), fields with ValueRank/ArrayDimensions/
  IsOptional; matrices row-major; identity = DataType NodeId, no version field.
- Sparkplug B 3.0: no enums; Templates = UDTs defined in NBIRTH; DataSet row-wise;
  aliases replace names after BIRTH; quality property 0/192/500.
- DDS XTypes: enum, bitmask, struct, union, sequence, array, map, @optional/@key/@id;
  FINAL/APPENDABLE/MUTABLE; type identity = MD5 prefix of TypeObject.
- ROS 2 IDL has no enums; MCAP schema records referenced by u16 ID.
- Arrow: struct = validity + child arrays; list, fixed-size list, dictionary, sparse/
  dense unions, run-end, StringView; extension types (fixed_shape_tensor, json, uuid).
- Avro: name-resolved fields, promotions int->long->float->double, canonical form +
  fingerprint; Confluent default BACKWARD.
- CAN DBC: VAL_ tables as enums, scale/offset/min/max/unit per signal, multiplexing.
- Sift: enum (name, u32 key) and bit-field types; no structs (protobuf flattened to
  scalar channels); adding an enum variant makes a new channel version and breaks
  Rules, Sift advises strings while value sets change. LESSON: keep enum members out
  of channel identity.
- InfluxDB 3: float, int, uint, string, bool only. Synnax: no enums or structs.
- Layout: columnar SoA (Arrow, Polars, Rerun, Flight) lets each field compress alone,
  SIMD decode, project one field; costs per-column framing on tiny batches and a pivot
  for row sinks (MQTT JSON, Avro, OPC UA writes).
- Compression (ALP paper, bits per f64): Gorilla 48.1, Chimp 42.6, Chimp128 24.5,
  Elf 18.2, ALP 16.4, Zstd 17.2; ALP ~55x faster decode than Gorilla. FastLanes
  1024-value vectors. pcodec 29-94% better ratio than Parquet/Zstd, >1 GiB/s, wants
  chunks >10k (at rest, not transport). Parquet added ALP (encoding 10) in Preview
  2026-08-01. ClickHouse ALP Beta. Crates: pco 1.0.3, fastlanes 0.7.2, alp 0.0.4,
  vortex 0.87, fsst-rs 0.6, bitpacking 0.9; tsz/gorilla stale.
- Timestamps: raw i64 at 100 kHz = 800 kB/s per channel; shared index + exact stride
  removes it. Trap: 10.24 kHz gives dt = 97,656.25 ns; use a rational stride
  t_i = t0 + floor(i*num/den), else delta-of-delta costs ~2 bits per sample.
- NI waveform: t0 + dt + Y, REGULAR/IRREGULAR timing, raw data + linear scale + units
  per waveform.
- Units/quality: OPC UA EUInformation (UNECE), EURange; DataValue StatusCode 32-bit
  per sample. Escape hatches: Synnax json/bytes, Sparkplug Bytes, Arrow json/opaque,
  Parquet VARIANT. Risk: an escape hatch silently becomes the default.
- Agent recommendation: primitives bool, i8-i64, u8-u64, f32, f64, timestamp,
  duration, string, bytes, uuid, flags<uN>; open enums with explicit backing int;
  structs matched by name with aliases; fixed arrays/tensors, bounded lists, optional
  via validity bitmap, dense unions; no maps; Arrow-compatible SoA in memory, compact
  wire frames with per-leaf encoded buffers, session schema IDs; type ID = SHA-256 of
  structural canonical form; BACKWARD_TRANSITIVE in CI; units/ranges per field;
  scaling per channel (store raw counts); quality optional per-sample u32 column
  (OPC UA semantics, absent = Good); bytes requires a media_type label.

## 9. Rust ledger part 2 (verified 2026-10-04)

- Raft: openraft 0.10.0-alpha.36 (active; 0.9.25 fixes only). Pluggable storage,
  network (RaftNetworkV2), runtime (AsyncRuntime incl. single-threaded); joint
  membership, learners, pre-vote; turmoil fuzzer in CI; Jepsen per push; users
  Databend, CnosDB, RobustMQ. API unstable, on-disk format can change. raft-rs 0.7.0
  (2023 release, master fixed 2026-05) is a deterministic tick-driven core, TiKV.
  Verdict: openraft pinned `=0.10` over an iroh ALPN.
- iroh 1.3.0 uses noq 1.3 (n0 fork of Quinn: multipath, NAT traversal). Embedded relay
  via iroh-relay `server` feature (forces tls-ring, pulls ACME/clap/toml); access
  control hook can admit only mesh keys. `Builder::empty()` / RelayMode::Custom avoid
  n0 infra. Datagrams and SendStream::set_priority(i32). AddressLookup trait,
  EndpointHooks. No first-class sim: `unstable-custom-transports` exists but time is
  not injectable (issue #4459 open), custom transport bugs (#2676 open, #4144).
  Cleanest seam: Foundation's own Transport trait with iroh and sim implementations;
  test real iroh in patchbay (Linux netns, non-deterministic).
- DST: turmoil 0.7.2 with `unstable-fs` (pending vs durable writes, torn writes,
  EIO, corruption, ENOSPC, crash/bounce); mad-turmoil 0.2.1 interposes clock and
  randomness (Linux). Inject Clock, Rng, Fs, Transport; current-thread runtime;
  run-twice-and-diff self-check. Antithesis SDK 0.3.0 later.
- MCP: rmcp 3.5.0 (fast majors; pin). ToolAnnotations: title, readOnlyHint (false),
  destructiveHint (true), idempotentHint (false), openWorldHint (true); spec
  2026-07-28 confirms names; clients treat annotations as untrusted.
- Mesh file: TOML (toml 1.1.6, TOML 1.1 spec; toml_edit 0.25.15 format-preserving;
  taplo/tombi validate via JSON Schema). serde_yaml deprecated, serde_yml unsound.
  KDL 2.0 via kdl 6.7.1. CUE needs cgo. schemars 1.2.2 (JSON Schema 2020-12).
- Observability: tracing 0.1.44; OTel Rust metrics/logs stable, traces beta;
  prometheus-client 0.25.1 with injected Registry (no global recorder in DST).
- CLI: clap 4.6.7, clap_complete, clap_mangen; one Command tree drives docs,
  completions, and MCP tool list; handlers return Serialize output.
- Local IPC: Unix socket / named pipe (interprocess 2.4.4); Python grpcio has no
  named pipes on Windows; zenoh-python embeds the full node via PyO3. Verdict: daemon
  + thin Rust client over length-prefixed frames; Python SDK is a maturin abi3 wheel
  around the client. iceoryx2 0.10.0 later for zero-copy local.
- WAL: own segmented WAL, frames with length, seq, CRC32C; group commit; fsync file
  and parent dir; truncate only the active segment tail on recovery; sealed-segment
  corruption repaired from peers; fsync error = crash and recover, never retry; Rust
  sync_all maps to F_FULLFSYNC (Apple) and FlushFileBuffers (Windows). References:
  TigerBeetle journal, Kafka segment recovery, fjall journal. okaywal not production.
- Clock offset: NTP four-timestamp exchange; write own module with min-delay filter
  and ClockBound-style (earliest, latest). ClockBound itself is Linux-only and uses
  local sources, not peers.
- Riskiest: real iroh under DST; "every node can relay" (needs public address, OT
  blocks UDP, armv7 not in iroh CI); Raft voters over relayed links; one WAL durable
  on three OSes; latest-mode over datagrams (~1.2 KB cap, relay behavior unverified)
  and clock bounds over asymmetric relayed paths.
