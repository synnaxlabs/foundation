# Foundation research 14: whole-architecture alternatives

Fork r14, 2026-10-04. Scope: compare Foundation's overall shape against other shapes
that move industrial data, and decide whether Foundation should build on one of them
(Zenoh in particular). Inputs: the decision log (`memory/project_foundation_design.md`),
research reports r1, r2, r4, r5, r6, r8, the Synnax code in this repo, web sources, and
local measurements made for this report.

Marks used on claims:

- **[V2]** two or more independent sources agree.
- **[V1]** one primary source (owner docs, code, spec, or a peer-reviewed paper).
- **[VENDOR]** the claim comes from the vendor or project that sells or ships the
  system. Treat it as an upper bound.
- **[M]** measured for this report (section 2).
- **[U]** unverified: inference, a secondary summary, or a negative ("not found").

---

## Summary

**Verdict.** Foundation's shape is the right base. No alternative gives the
combination Foundation needs: a durable, ordered, per-source log that lives where the
data is born, with reader positions; one control gate per index with authority and
leases; time with an error bound; and fleet-wide plan and apply. Each alternative has
one or two of these, never all. Building on Zenoh would replace one or two crates
(transport and part of routing) and would cost the T1 simulation rule, the C2 shard
model, and a second routing model. It does not pay.

**But the shape has real weak points**, ranked:

1. **Too much new machinery at version 0.** Own QUIC transport and relays, own Raft,
   own spec tree with branch delegation, own storage and codecs, own protocol clients,
   and a thread-per-core runtime, all at once. Every alternative stands on one proven
   core. This is the largest schedule and correctness risk.
2. **WAN fan-out.** A complete reader pulls from the home. The home is usually the edge
   node. N remote durable readers mean N streams and N holds across the weak link.
   Brokers, MQTT, Zenoh routers, and PI to PI all send once over the weak link and fan
   out near the readers.
3. **The userspace QUIC bet.** Zenoh's QUIC link measured 0.7 Gbit/s against 13 to
   19 Gbit/s for its TLS-over-TCP link on the same machine, with 1.6x to 3.6x the
   one-way latency [M]. P1 needs about 3.2 Gbit/s to one remote complete reader. The
   r5 literature gives 2.4 to 8.2 Gbit/s per QUIC connection, bound by one core.
4. **One home per index.** Failover and its durability contract are not designed yet
   (r13 is open). Until a reader takes the data, the edge disk is the only copy.
5. **Missing paths every incumbent has**: a quarantine for data a sink rejects
   forever, a record that tells "dead" from "quiet", and lossy reduction (deadband,
   swinging door) for weak links.

**Scores** (0 none, 1 weak, 2 partial, 3 strong; Foundation is scored on design
intent, with nothing built):

| Shape | Transport and networking | Connectors and time sync | Outbound integrations | Agent-operable | Mesh as code | P1 | Maturity |
|---|---|---|---|---|---|---|---|
| Foundation (design) | 3 | 3 | 2 | 3 | 3 | 2 (unproven) | 0 |
| Zenoh | 3 | 1 | 1 | 1 | 1 | 2 | 3 |
| DDS (Connext, Cyclone, Fast DDS) | 2 | 1 | 1 | 1 | 1 | 2 | 3 |
| Broker cluster plus edge agents | 1 | 1 | 3 | 2 | 2 | 1 | 3 |
| Historians (PI, Ignition, Canary) | 1 | 2 | 2 | 1 | 1 | 0 | 3 |
| MQTT, Sparkplug B, UNS | 2 | 1 | 2 | 1 | 1 | 1 | 3 |
| Synnax Core plus Driver | 1 | 2 | 0 | 1 | 1 | 2 | 2 |
| Aeron | 2 | 0 | 0 | 1 | 1 | 3 (latency) | 3 |
| Pipelines (Vector, Redpanda Connect, UMH Core) | 1 | 1 | 3 | 2 | 2 | 1 | 2 |

Justification per cell is in section 4.6.

**Decisions** (full text in section 9): keep the shape; do not build on Zenoh, add a
Zenoh connector later; add read copies near readers; measure QUIC against TLS over TCP
on Linux before locking the transport; add quarantine; add a node death record; add
lossy reduction functions; check reader and writer settings at plan time; cut the
first phase's novelty; gate idle memory from phase one; add an alternatives section to
the RFC.

---

## 1. What is being compared

Foundation's shape, as locked or proposed in the decision log:

1. A mesh of identical nodes, one binary (D7).
2. Channels with one **home** per index. The home orders samples, numbers them (seq),
   stores them in a bounded disk buffer, gates control, and delivers to readers (A1,
   A7, A8, B1 to B7, S11).
3. A Raft group per voter branch holds the spec pointer and runtime state (homes,
   leases). The spec is a content-addressed tree fetched from any node (S9, K5, r4).
4. Thread-per-core shards own indexes. Connectors run in process through `hub` (C1, C2,
   r1).
5. Config as code with discover, plan, apply, explain, export (K1 to K5, r3).
6. A mover, not a database: hold and retention are bounded (D1, S10).
7. Own transport on noq-proto (QUIC) with relays and TCP carriers (r5); own mesh clock
   with error bounds (C6, r6).
8. Deterministic simulation of the whole mesh (T1).

A whole-shape comparison must answer five questions for each alternative:

- Where does data first become durable, and who holds it until each consumer has it?
- Who orders samples, and who decides which writer may control a device?
- How does config reach nodes, and can a cut-off site change its own config?
- What crosses a weak link, how often, and what is lost when it breaks?
- What time do the timestamps carry, and with what error?

---

## 2. Local measurements (this study)

Machine: Apple M3 Max, 16 cores, 48 GB, macOS 27.0, rustc 1.98.1. Loopback only, two
processes, no CPU pinning, one run each. Code and raw output:
`scratchpad/r14/zbench/` (session scratchpad; copy before reboot).

### 2.1 Idle footprint [M]

Each server started on a free port with default settings, measured with `ps` after
about 20 to 50 s of idle.

| Process | Idle RSS | Binary size (macOS arm64 / Linux arm64) | Notes |
|---|---|---|---|
| `zenohd` 1.10.1 (router, no plugins loaded) | 9.5 MB | 14.2 MB / 16.4 MB | 4 threads at idle |
| `nats-server` 2.15.0 with JetStream on | 17.2 MB | 17.5 MB / 17.3 MB | file store in scratch dir |
| Synnax Core (repo build, embedded Driver off, insecure, empty data dir) | 74 MB | 125 MB / n/a | the binary embeds Console assets [U] |

Reading: a Rust or Go server can idle well under the 50 MB P1 target. Synnax shows how
easily a full product misses it. The P1 idle gate should run from phase one
(decision 10).

### 2.2 Zenoh latency [M]

Ping-pong between two Zenoh 1.10.1 peer processes over loopback: `express(true)`,
`CongestionControl::Block`, 5,000 warm-up rounds, 50,000 measured, one-way = RTT / 2.
TLS and QUIC use an ECDSA P-256 leaf signed by a local CA. Baseline: plain TCP with
`TCP_NODELAY` and a length prefix.

| Path | Payload | p50 (us) | p99 (us) | p99.9 (us) | max (us) |
|---|---|---|---|---|---|
| Plain TCP | 64 B | 22.0 | 26.4 | 56.3 | 2,640 |
| Zenoh TCP | 64 B | 28.4 | 43.2 | 58.2 | 136 |
| Zenoh TLS | 64 B | 30.1 | 43.7 | 71.4 | 127 |
| Zenoh QUIC | 64 B | 49.4 | 74.2 | 102.0 | 5,026 |
| Plain TCP | 8 KiB | 21.4 | 27.9 | 44.0 | 63 |
| Zenoh TCP | 8 KiB | 32.4 | 50.6 | 68.9 | 156 |
| Zenoh TLS | 8 KiB | 37.0 | 58.7 | 77.7 | 5,026 |
| Zenoh QUIC | 8 KiB | 132.7 | 170.4 | 202.2 | 3,799 |

### 2.3 Zenoh throughput [M]

One publisher process in a tight `put` loop, one subscriber process counting for 5 s
after 1 s of warm-up. Default batching (adaptive, driven by back-pressure).

| Link | 8 B | 1 KiB | 64 KiB |
|---|---|---|---|
| TCP | 11.4 M msg/s (0.73 Gbit/s) | 3.7 M msg/s (30.4 Gbit/s) | 37 k msg/s (19.6 Gbit/s) |
| TLS | 11.6 M msg/s (0.74 Gbit/s) | 2.3 M msg/s (18.7 Gbit/s) | 25 k msg/s (13.0 Gbit/s) |
| QUIC | 6.2 M msg/s (0.40 Gbit/s) | 85 k msg/s (0.69 Gbit/s) | 1.3 k msg/s (0.70 Gbit/s) |

### 2.4 What the numbers mean

- Zenoh's TCP and TLS links add about 6 to 16 us at p50 and 17 to 31 us at p99 over
  raw loopback TCP. The transport is not a reason to avoid Zenoh. It is fast.
- 8-byte messages top out near 11 M/s, which matches the project's own 10.7 M msg/s
  claim for 1.5.0 on MacBooks [VENDOR, now M]. Per-message designs are two orders below
  P1's 100 M samples/s. Batched series (Foundation, Synnax, Kafka record batches) are
  the only way to P1.
- Zenoh's QUIC link was 19x to 28x slower than its TLS link on bulk data and 1.6x to
  3.6x slower on latency. Caveats: macOS has no UDP GSO/GRO, Zenoh's QUIC uses default
  windows, and loopback hides NIC offloads. This is not a general QUIC ceiling. It is a
  warning that a QUIC-only data path is a P1 risk until measured on Linux (decision 4).
- All tails here are far below P1's 250 us one-hop p99, except QUIC at 8 KiB, which
  already uses 170 us on loopback before any wire time.

---

## 3. The alternatives

Each section uses the same headings. "Take" lists what Foundation should adopt.

### 3.1 Eclipse Zenoh

**Architecture.** One protocol and one Rust library in three roles: peers (mesh),
clients (attach to one router), and routers (forward between links). Keys are
`/`-separated with `*` and `**` wildcards. Primitives: publisher, subscriber,
queryable (a computation answering `get` on a key expression), storage (a subscriber
plus queryable that keeps the latest value per key), and liveliness tokens. Links:
TCP, TLS, QUIC, UDP, WebSocket, serial, Unix sockets, and shared memory. Eight
priority queues per link, adaptive batching under back-pressure, fragmentation, and an
`express` flag. Interceptors at routers: access control, downsampling, low-pass filter.
Timestamps are hybrid logical clocks. The 1.0 wire format has a published spec. Rust,
C (zenoh-c), C++, Python, Kotlin, TypeScript bindings, and Zenoh-Pico for
microcontrollers. Sources: zenoh.io abstractions page; DEFAULT_CONFIG.json5 at tag
1.10.1; spec.zenoh.io 1.0.0 [V1].

**Performance evidence.**

- 2023 study (NTU, published on zenoh.io and arXiv 2303.09419; Ryzen 7 5800X at a fixed
  4.0 GHz, 100 GbE, Zenoh 0.7.0-rc, Mosquitto 2.0.15, Kafka 3.2.1 with acks=0, Cyclone
  DDS): Zenoh P2P over 4 M msg/s for small payloads and up to 67 Gbit/s on one
  machine; 50 Gbit/s P2P and 34 Gbit/s brokered between machines; 64-byte latency 10 us
  one machine, 16 us between machines; brokered 41 us [V1, project-published].
- 1.5.0 release: about 10.7 M msg/s at 8 bytes on MacBook loopback, "nearly doubling"
  since 1.0.0; shared memory about 3 M msg/s [VENDOR].
- 1.10 release: io_uring receive path on Linux cuts round-trip latency about 24% at 64 B
  and 18% at 64 KiB [VENDOR].
- This study: section 2 [M].

**Operations and agents.** One router binary; config in JSON5 per process; runtime
changes through the "admin space" (key prefix `@`) and the REST plugin. ACL config
"cannot be updated at runtime, and requires a restart" (Zenoh access control docs)
[V1]. A commercial monitor (ZettaC2) exists [VENDOR]. No MCP server found [U].

**Config as code.** Per-node JSON5 files and environment variables. No fleet model, no
plan, no diff against a running mesh.

**Edge failure behavior.** Reliability is per hop: each session has a reliable and a
best-effort channel (zenoh.io reliability blog, 2021) [V1]. Per-link TX queues hold 2
batches of 64 KiB per priority. A `Drop` publisher waits `wait_before_drop` = 1,000 us
for a free batch, then drops the message. A `Block` publisher waits up to
`wait_before_close` = 5 s, then the transport session closes (DEFAULT_CONFIG.json5
lines 646 to 658) [V1]. rmw_zenoh's docs warn that large ROS messages "can disappear
without any error" under load because of the 1 ms default [V1]. End-to-end recovery is
opt-in through `zenoh-ext`'s AdvancedPublisher: an in-memory cache bounded by
`max_samples`, heartbeat-based miss detection, and subscriber recovery; 1.10 adds a
one-hour default retention of per-publisher state [V2: docs.rs zenoh-ext and the 1.10
release notes]. Storages are latest-value key-value stores; replicas converge by an
"eventually consistent" alignment protocol (zenoh.io alignment blog, 2022) [V1]. There
is no durable, ordered, per-reader log. No NAT hole punching found; peers reach each
other through routers with reachable addresses, discovered by scouting or gossip [U:
negative finding].

**Control.** No write arbitration, no authority, no lease on a writer. Queryables give
request and reply; liveliness tokens give presence.

**Time.** HLC: 64-bit time plus a unique ID; ordering, not synchronization. `uhlc`
rejects a timestamp more than 500 ms ahead of local time by default (docs.rs uhlc)
[V1]. Zenoh relies on NTP or PTP outside itself.

**Footprint.** `zenohd` idles at 9.5 MB RSS [M]. Zenoh-Pico runs on ESP32 and STM32
[V1].

**License.** EPL-2.0 OR Apache-2.0 [V1]. ZettaScale sells support and a QNX build
[VENDOR].

**Maturity.** ROS 2 Kilted Kaiju (May 2025) made rmw_zenoh a Tier 1 middleware
(docs.ros.org Kilted release page; Open Robotics blog) [V2]. 1.x has a stable wire spec.

**Better than Foundation's shape.** A proven, fast, multi-link transport today.
Routers fan out one upstream flow to many local subscribers. Shared memory for local
processes. A microcontroller client. Bindings in six languages. A large robotics user
base.

**Take.**

- Multi-link from day one, with TLS over kernel TCP as the fast default (decision 4).
- Back-pressure-driven batching: validates B6 smart batching. An `express` setting per
  writer for commands.
- Subscription aggregation at a node: one upstream flow per remote home, fanned out
  locally (decision 3).
- Per-hop timestamp instrumentation (1.10 "Send, Route, Receive" stack) as a debug
  option for latency breakdowns.
- Lesson: a silent 1 ms drop is the failure users hit. Every drop in Foundation must be
  an explicit gap with a counter (B5 already says so; keep it absolute).
- Later: a shared-memory local path for SDKs and a small C client for
  microcontrollers.

### 3.2 DDS (RTI Connext, Eclipse Cyclone DDS, eProsima Fast DDS)

**Architecture.** Brokerless, data-centric pub/sub over RTPS. Topics with keyed
instances; every DataWriter and DataReader carries QoS: RELIABILITY, HISTORY
(KEEP_LAST n or KEEP_ALL), DURABILITY (VOLATILE, TRANSIENT_LOCAL, TRANSIENT,
PERSISTENT), DEADLINE, LIVELINESS, OWNERSHIP and OWNERSHIP_STRENGTH, LIFESPAN,
DESTINATION_ORDER. Discovery is peer to peer, usually over UDP multicast. XTypes for
type evolution. DDS Security for authentication and access control.

**Performance evidence.**

- Cyclone DDS README: over 1 M samples/s reliable for very small samples, about 90% of
  GbE at 100 bytes, latency about 30 us (Xeon E3-1270 v2, Ubuntu 16.04) [V1, project].
- TUM and Siemens, Middleware 2023 (cross-vendor DDS-Perf tool): Connext best
  all-round bandwidth and peak sample rate; Fast DDS best end-to-end latency [V1, peer
  reviewed].
- 2023 Zenoh study: Cyclone DDS about 2 M msg/s and 26 Gbit/s on one machine, 14 Gbit/s
  between machines; latency 8 us and 37 us [V1, project-published].

**Operations and agents.** Per-application XML QoS profiles; vendor tools (RTI Admin
Console, Fast DDS Monitor). QoS mismatch prevents communication; it is reported only
through OFFERED and REQUESTED_INCOMPATIBLE_QOS status, which applications often do not
watch (RTI API docs) [V1]. ROS 2 moved to Zenoh as Tier 1 partly because DDS discovery
sends O(N²) messages and depends on multicast, which many networks block (ROSCon 2024
rmw_zenoh talk; Open Robotics alternative middleware report) [V2].

**Config as code.** XML QoS profiles and, for Connext, XML application creation. Per
application, not per fleet.

**Edge failure behavior.** Reliable RTPS repairs loss with heartbeats and
ACKNACKs while the writer's history holds the sample. TRANSIENT and PERSISTENT
durability need a separate Persistence Service process (RTI docs) [V1]. Over WAN:
Connext's Real-Time WAN Transport plus Cloud Discovery Service for NAT traversal
(Connext 6.1+) [V1, VENDOR]; open-source users bridge DDS over WAN with routers such
as Zenoh's DDS plugin.

**Control.** The closest precedent to Foundation's gate. EXCLUSIVE ownership lets one
writer per instance win by OWNERSHIP_STRENGTH; LIVELINESS gives a lease; DEADLINE gives
an expected period. But each DataReader arbitrates on its own (RTI ownership docs)
[V1], so two readers can disagree for a while about who owns an instance. Foundation's
home makes one decision and publishes it (S11).

**Time.** Source timestamps and DESTINATION_ORDER BY_SOURCE_TIMESTAMP; no clock
synchronization in the standard.

**Footprint.** Small: Cyclone's library installs at 1.4 MB (Debian package) [V1];
Connext Micro targets microcontrollers [VENDOR].

**License.** Cyclone EPL-2.0 or EDL-1.0 [V1]; Fast DDS Apache-2.0 [V1]; Connext
commercial, priced per developer with royalty-free runtime [VENDOR].

**Better than Foundation's shape.** Microsecond latency with no broker; a precise,
standard QoS vocabulary; decades in defense and robotics.

**Take.**

- Vocabulary check: Foundation's latest mode = KEEP_LAST 1 plus TRANSIENT_LOCAL;
  max age = LIFESPAN; lease = LIVELINESS; authority = OWNERSHIP_STRENGTH. Cite these
  in the RFC so DDS users map concepts quickly.
- DEADLINE: an expected interval per connector group, with a status record when a
  source goes silent (folded into decision 6).
- Lesson: settings that silently prevent communication are the top DDS complaint.
  Foundation can check every setting pair at plan time because all settings live in the
  spec (decision 8).
- Lesson: never depend on multicast discovery. Foundation's membership from the spec
  already avoids it.

### 3.3 A broker cluster plus edge agents

**Architecture.** A central, replicated log (Kafka, Redpanda, NATS JetStream) and
agents at the edge (Telegraf, the OpenTelemetry Collector, Vector, Kafka Connect
workers). Agents poll or subscribe to devices, buffer locally, and produce to the
broker. Consumers read from the broker with committed offsets. NATS adds leaf nodes: an
edge server that keeps local traffic running and connects out to a hub.

**Performance evidence.**

- Jack Vanlightly (then at Confluent), 2023, 3 brokers on i3en.6xlarge: Kafka saturated
  the NVMe drives at 2 GB/s; Redpanda reached 1 GB/s but end-to-end latency rose to
  24 s with 50 producers; the result contradicts Redpanda's 2022 benchmark (Vanlightly,
  "Kafka vs Redpanda Performance") [V1; the author worked for a competitor, and
  Redpanda published a rebuttal].
- Apache Iggy (Rust) after moving to thread-per-core: P99 4.52 to 1.82 ms at about
  1,000 MB/s (r1) [VENDOR].
- NATS core: 7.0 to 7.7 M msg/s for 16-byte messages from one publisher (NATS docs
  example) [VENDOR].
- 2023 Zenoh study: Kafka 56 to 63 k msg/s small payloads, 4 to 5 Gbit/s, latency 73
  to 81 us with acks=0 [V1, project-published].

**Operations and agents.** Strong CLIs and Terraform providers for broker resources.
MCP servers exist: `rpk connect mcp-server` in Redpanda Connect [V1], community NATS
MCP servers [U]. Agents are configured per node (TOML, YAML).

**Config as code.** Broker objects through Terraform or APIs; agents through local
files distributed by Ansible or Kubernetes. No single plan across the brokers and the
fleet of agents.

**Edge failure behavior.** Two durability domains: the agent's buffer, then the
broker's log.

- Telegraf: disk buffer survives restarts, but its write-ahead logs "can grow until the
  filesystem fills" (OneUptime guide, 2026) [U: secondary].
- Vector: disk buffers sync every 500 ms by default; Vector exits on a flush I/O error
  (Vector buffering model) [V1].
- OTel Collector: persistent queue writes a WAL through the `file_storage` extension;
  docs say guarantees "might not be as strong as dedicated message queues" [V1].
- NATS leaf nodes: core NATS has no store and forward over the leaf link; JetStream
  mirrors and sources resume 10 to 20 s after reconnect (NATS leaf node docs) [V1].
- JetStream durability: Jepsen 2025-12-08 found that the default fsync every two
  minutes lost 131,418 of 930,005 acknowledged writes (14.1%) in a coordinated power
  failure; a single-bit error on one of five nodes lost 49.7% (jepsen.io) [V2: Jepsen
  plus Synadia's response].

**Control.** None built in. Commands are messages; NATS request and reply exists. No
authority, no gate, no lease. The command path usually goes through the cloud.

**Time.** Producer or broker timestamps; no synchronization.

**Footprint.** Kafka KRaft: Confluent recommends at least 4 GB RAM and a 1 GB heap [V1].
Redpanda: at least 2 cores and 2 GB per core, x86-64 or Graviton (Redpanda
requirements) [V1]. NATS with JetStream: 17 MB idle [M].

**License.** Kafka Apache-2.0. Redpanda Community BSL 1.1, converting to Apache-2.0
after four years [V1]. NATS Apache-2.0; Synadia's 2025 attempt to move it to BSL was
reversed and the trademark went to the Linux Foundation (CNCF blog, 2025-05-01) [V2].
Telegraf MIT, OTel Apache-2.0, Vector MPL-2.0.

**Better than Foundation's shape.** The best ecosystem of sinks and consumers. Mature,
tested replication. Data crosses the weak link once and then fans out in the cloud.
Consumer positions are a solved problem.

**Take.**

- Send once across the weak link, fan out near readers (decision 3). Kafka MirrorMaker
  and JetStream mirrors are the precedent.
- Jepsen's lesson: never acknowledge before fsync by default. Foundation already gives
  complete readers frames only after disk sync (A8); state the writer-side ack contract
  as plainly (r13).
- Vanlightly's lesson for P1: benchmark with many producers, long runs, and fsync on,
  and publish the setup.
- NATS shows the whole shape can be small: leaf nodes, accounts, subjects, and a Raft
  log in a 17 MB binary.

### 3.4 The industrial historian model: AVEVA PI, Ignition, Canary

**Architecture.**

- **PI**: interfaces and adapters at the edge collect data, apply exception reporting
  (deadband), and buffer through the PI Buffer Subsystem. The PI Data Archive applies
  swinging-door compression and stores. Collectives are two or more archives; the
  buffer sends to each member on its own queue ("n-way buffering"). Config tables
  replicate from the primary; "You can only change values of replicated tables at the
  primary server" (AVEVA docs) [V1].
- **Ignition**: Java gateways with drivers, tags, a store-and-forward engine (memory
  buffer, disk cache, quarantine), the Gateway Network between gateways, and the
  Enterprise Administration Module (EAM), where a controller gateway pushes projects
  and backups to agent gateways (Ignition 8.3 docs) [V1].
- **Canary**: Windows services. Collectors write to a local Sender with a store and
  forward cache; a Receiver feeds the historian (Canary help center) [V1].

**Performance evidence.**

- PI Server 2024 R2 topology envelope: archive rate 25,000 events/s, inbound 50,000
  events/s for one topology (AVEVA topology docs, via search summary) [VENDOR, U]. PI
  3.4-era material: up to 80,000 events/s [VENDOR].
- Canary: 2.8 M TVQ/s raw write; over 3.6 M updates/s on the server (Canary blog, 2016)
  [VENDOR].
- Ignition: guidance of 16 GB RAM for 10,000 value changes/s on one server [U:
  secondary summary of IA material].
- Data reduction: an AIChE 2023 study of exception plus swinging-door compression
  reported 80% fewer stored values with small error [V1].

**Operations and agents.** GUI-first. Ignition 8.3 moved all gateway config to
human-readable files, added a REST config API with OpenAPI, and "deployment modes"
that map one config onto dev, test, and production addresses [V1]. An Ignition MCP
module is in early access [V1]. PI adapters are configured through a local REST API
with JSON files [V1].

**Config as code.** Ignition 8.3 files plus Git is the strongest incumbent example, but
per gateway; EAM pushes projects, not a planned diff of the whole system. PI adapters
JSON per adapter.

**Edge failure behavior.** The incumbents' strongest area. PI buffers per archive
member and sends in order on reconnect [V1]. Ignition's store and forward retries;
records that fail too often go to **quarantine**, where a person can delete or retry
them (Ignition 8.3 docs) [V1]. Ignition redundancy (master and backup) "does not claim
to be lossless" (Ignition redundancy docs) [V1].

**Control.** Writes through drivers and output points with security roles; redundancy
lets only the active gateway write. No multi-writer arbitration.

**Time.** PI to PI measures the clock offset between archives every two minutes and
adjusts timestamps; offsets up to 15 minutes count as drift (AVEVA PI to PI docs) [V1].
Others take source or gateway time.

**Footprint.** PI Data Archive and Canary run on Windows. Ignition Edge on ARM needs at
least 2 GB RAM, with a 1.47 GB download (secondary install guide) [U].

**License.** Commercial. Ignition: per server, unlimited tags. Canary: per tag or
unlimited. PI: enterprise agreements.

**Better than Foundation's shape.** Decades of field-proven store and forward, the
widest protocol catalogs, and data reduction at the edge. Operators trust them.

**Take.**

- **Quarantine** (decision 5).
- **Deadband and swinging-door compression** as opt-in calculation functions
  (decision 7).
- **n-way buffering**: the source sends to each copy on its own queue, with no
  consensus on the data path. Precedent for r13 and for decision 3's read copies.
- **Offset correction between nodes on a fixed period**: confirms C6's direction.
- **Honest redundancy contract**: state in the RFC what failover may lose.
- **Deployment modes**: one definition set, environment-specific addresses. Foundation
  can express it with branches; check K2 covers it.

### 3.5 MQTT with Sparkplug B and the Unified Namespace

**Architecture.** A broker (HiveMQ, EMQX, Mosquitto) as the hub; edge nodes publish on
a topic tree. Sparkplug B adds protobuf payloads, birth and death certificates
(NBIRTH, NDEATH as the MQTT will), sequence numbers, aliases (numbers in place of names
after birth), commands (NCMD, DCMD), and a Primary Host STATE message. The UNS is a
convention: one hierarchical namespace for the whole enterprise on the broker.

**Performance evidence.**

- EMQX 5.0: 100 M connections on 23 nodes; about 1 M msg/s in and out, one message per
  publisher every 90 s; replicant nodes at 90% memory (113 GiB) and 97% CPU (EMQX blog)
  [VENDOR].
- HiveMQ 4.11: 200 M connections on 40 nodes, peak 1 M PUBLISH/s (HiveMQ benchmark
  page) [VENDOR].
- 2023 Zenoh study: Mosquitto 33 to 38 k msg/s small, about 9 Gbit/s large; latency 27
  and 45 us [V1, project-published].
- Brokers are built for connection counts, not sample density: 1 M msg/s across 23 to
  40 nodes is 25 to 45 k msg/s per node.

**Operations and agents.** Mature brokers with REST admin APIs. UNS needs naming
governance; "there is no universal standard for the Unified Namespace" (IIoT World)
[U: secondary].

**Config as code.** Broker config files; edge gateway config per vendor.

**Edge failure behavior.** Sparkplug data messages are QoS 0. An edge node may store
data while its Primary Host is offline and flush it marked `is_historical` when STATE
says online (Sparkplug 3.0, sections on Primary Host and Payload) [V1]. Only one
primary host per edge node. HiveMQ Edge's offline buffering is a commercial feature
[V1]. Rebirth storms after reconnects are a known pain (research ledger, section 1).

**Control.** NCMD and DCMD; the spec makes the Primary Application responsible for
commands but defines no authority, arbitration, or acknowledgment beyond the device
publishing its new value [V1].

**Time.** Epoch milliseconds, UTC [V2]. No synchronization.

**Footprint.** Mosquitto and HiveMQ Edge are small; EMQX and HiveMQ clusters are not.

**License.** Mosquitto EPL/EDL; HiveMQ CE and HiveMQ Edge open source with commercial
features; EMQX moved to BSL 1.1 at 5.9, and a cluster of more than one node needs a
license (EMQX license FAQ) [V1].

**Better than Foundation's shape.** Ubiquitous: every gateway, PLC vendor, and SCADA
speaks MQTT. Death certificates through the broker's will tell "dead" from "quiet"
with no extra protocol.

**Take.**

- **Death record** (decision 6).
- Aliases after birth validate A4's per-connection short keys.
- Primary Host STATE is a one-consumer special case of Foundation's per-reader holds
  (S10). Foundation's model is the general form; say so in the RFC.
- Resume by sequence, never by a full republish of state, to avoid rebirth storms.
- The UNS is a positioning gift: Foundation's name tree (A3) is a typed, durable UNS.
- The MQTT and Sparkplug connector is a first-wave must (D4 list).

### 3.6 Synnax: the Core cluster (Aspen, Cesium) plus the Driver

**Architecture.** A Go server ("Core") in four layers: storage (Cesium for telemetry,
Pebble for metadata), distribution (Aspen membership and KV, channels with node-aware
keys, a framer that routes reads and writes to the node that leases the data), service,
and API (`docs/claude/architecture.md`). A channel key is 12 bits of node key plus 20
bits of local key (`core/pkg/distribution/channel/channel.go:30-44`). The relay streams
live frames; a tapper aggregates the demands of all streamers into one tap per source
(`core/pkg/distribution/framer/relay/README.md`). The C++ Driver runs tasks
(NI, LabJack, OPC UA, Modbus, EtherCAT) as a client of the Core.

**Performance evidence.** Synnax blog, 2024-10-15, Apple M2 Max, Synnax v0.32, one
node: 73.7 M samples/s at 20 channels with 10 k samples per batch; with 1 k batches,
95.1 M at 200 channels, 94.3 M at 1,000, 72.3 M at 5,000, 47.5 M at 10,000; "As the
counts increase past 1,000, Synnax's performance begins to degrade"; Synnax "uses more
memory than alternatives"
(`site/docs/src/pages/blog/one-billion-rows/index.mdx`) [VENDOR]. Idle Core: 74 MB
[M].

**Operations and agents.** The Console GUI configures tasks; a CLI exists; no plan and
apply; config lives in the Core's metadata store.

**Config as code.** None for the fleet.

**Edge failure behavior.** Weak. The Driver's task manager reads its control stream
from the Core; when the read fails, the loop breaks and `stop_all_tasks()` runs
(`driver/task/manager.cpp:176-196`). There is no store and forward at the edge. Cesium
never calls `Sync` (grep of `cesium/`, no non-test `.Sync()` call) and keeps one file
per channel (blog). Aspen's README: "not yet ready for production use"; "The gossip
protocol lacks three essential features: failure detection, failure recovery, and
efficient propagation guarantees" (`aspen/README.md:34-46`).

**Control.** Authority 0 to 255 per channel with transfer to higher authority
(`x/go/control/authority.go:23-24`). The Driver treats a channel with no known state as
authorized: `all_authorized` returns true when the key is missing
(`driver/control/state.h:135-145`).

**Time.** The Driver's sample clock uses the PC clock; skew produces warnings only
(research ledger section 5; LabJack skew fields in
`core/pkg/service/labjack/versions/v2/types.gen.go:586-589`).

**Footprint.** 125 MB binary (macOS build in the repo), 74 MB idle [M].

**License.** BSL 1.1 (`LICENSE`).

**Better than Foundation's shape.** It exists, runs rocket test stands, and has a real
GUI. Its single-node throughput is near P1. Its channel and index model and its
authority model are proven with users.

**Take.** Foundation is already "Synnax's lessons, edge first" (A7, S11, research
ledger section 5). Two more:

- The relay's tapper is the precedent for decision 3's subscription aggregation.
- The blog's own curve (half of peak at 10 k channels) is the baseline that P1's "within
  2x at 100 k channels" must beat by 10x in channel count.

### 3.7 Other shapes

**Aeron (Real Logic, now Adaptive).** Reliable UDP unicast and multicast plus shared
memory IPC, Aeron Archive (record and replay streams by position), and Aeron Cluster
(Raft log plus a deterministic service). Evidence: about 10 us network round trip and
0.25 us IPC round trip for 100 bytes; 6 M msg/s at 40 bytes (High Scalability, older)
[VENDOR]; Aeron Cluster p50 30 to 99 us at 100 k msg/s of 288 bytes (STAC Summit 2024,
Adaptive) [VENDOR]. Apache-2.0; encryption (ATS) and kernel bypass are commercial
Premium features [V1]. No NAT traversal, no connectors, JVM first. **Take**: Archive's
"record a stream, replay from a position" is Foundation's buffer plus catch-up; Cluster's
"replicated log plus deterministic service with snapshots" is a candidate model for
home plus standby in r13; Aeron's offer-returns-back-pressure (never block the caller)
matches B5.

**Apache Arrow Flight.** Columnar record batches over gRPC. About 1 GB/s on one stream
on localhost, up to 6 GB/s for DoGet (Ahmad, Al-Ars, Hofstee, BID 2022) [V1]. Not a
mesh, no edge buffering, and S2 rejected Arrow layouts. **Take**: a Flight out connector
for analytics stores later.

**Pipelines: Vector, Redpanda Connect (Benthos), UMH Core.** One process per node, a
YAML or TOML pipeline (sources, transforms, sinks), local buffers, end-to-end
acknowledgments (Vector). Evidence: Vector's harness, TCP to blackhole 86 MiB/s vs
Fluent Bit 64 MiB/s (vendor harness, via secondary summary) [VENDOR, U]. Redpanda Connect
keeps the engine MIT and 223 connectors Apache-2.0, with some enterprise connectors;
WarpStream forked it as Bento [V2]. UMH Core is one container with embedded Redpanda,
Benthos-UMH (50+ industrial connectors), and an agent that polls `config.yaml` every
100 ms; Apache-2.0 except its management console; Core-to-Core links are on the
roadmap (UMH docs) [V1]. **Take**: UMH Core is the closest product competitor and shows
demand for "one box, a UNS, a buffer, YAML"; it is assembled from parts with a heavy
footprint (Redpanda's 2 GB per core guidance), and it has no mesh. Redpanda Connect's
MCP server and Vector's `tap`-style live inspection are agent-surface precedents for
C7 (a `foundation tap <selector>` operation).

**NATS as a whole shape.** Covered in 3.3; listed here because it is the closest
whole-shape precedent: identical servers, leaf nodes for outbound-only edges, subject
wildcards like Foundation's selectors, accounts and permissions like C8, streams with
Raft leaders like homes, mirrors for edge to cloud. It is Go (cannot be one Rust
binary), per-message (headers and subject per message), and has no control gate, time
service, or industrial connectors.

**OPC UA PubSub.** Brokerless UADP over UDP, Ethernet, or TSN, or brokered over MQTT;
aimed at field-level, controller-to-controller traffic (OPC Foundation) [V1]. A future
connector kind, not a base.

---

## 4. Cross-cutting comparison

### 4.1 Where data becomes durable, and who holds it for each consumer

| Shape | First durable point | Held for each consumer by | Acked to the source when |
|---|---|---|---|
| Foundation | The home's disk, usually the edge node (B1, B7) | Reader position plus hold at the home (S10) | Live: never waits (B5); complete readers see data after fsync (A8) |
| Zenoh | None by default; opt-in publisher cache in memory | Nothing durable; storages keep latest values | Per hop |
| DDS | Writer history in memory; Persistence Service if configured | Writer history and resource limits | Per reader ACKNACK |
| Broker plus agents | Agent buffer, then the broker log | Consumer offsets in the broker | After broker replication (if configured) |
| PI | Buffer Subsystem queue on the interface node | One queue per archive member | Archive accepts |
| Ignition | Store-and-forward disk cache | One engine per destination | Database accepts; quarantine after repeated errors |
| Sparkplug | Edge node's own store, while the primary host is offline | One primary host | QoS 0: never |
| Synnax | Core's Cesium (no fsync) | Nothing at the edge | Core accepts |

Reading: Foundation and PI are the only shapes whose first durable point is at the
source with a per-consumer hold. Brokers hold per consumer, but only after the data has
crossed the weak link to the central log.

### 4.2 Who orders samples, and who arbitrates control

| Shape | Order | Control arbitration |
|---|---|---|
| Foundation | Home per index, seq per path (A8) | Home gate: authority, first holder wins ties, writer lease, handoffs on a channel, unknown = not in control (S11) |
| Zenoh | Per publisher, per hop; HLC across sources | None |
| DDS | Per writer; optional by source timestamp | Ownership strength, decided by each reader |
| Broker | Partition leader | None |
| PI | Archive per point | None |
| Sparkplug | seq 0 to 255 per edge node | "Primary Application" by convention |
| Synnax | Leaseholder per channel | Authority per channel; Driver treats unknown as authorized |

Reading: only Foundation and Synnax arbitrate control at one point, and Foundation
fixes Synnax's unknown-state behavior.

### 4.3 What crosses a weak link

| Shape | Per sample cost | Copies across the link for N remote consumers | On outage |
|---|---|---|---|
| Foundation (as locked) | Series payload only (S2) | N complete streams, one per durable reader | Buffer at the home; explicit gaps past budget |
| Foundation with decision 3 | Same | 1 to the read copy, then local fan-out | Same |
| Zenoh | Per message header plus key ID | 1 per router link | Drop after 1 ms or block then close after 5 s |
| Broker plus agents | Per message or per record batch | 1 to the broker | Agent buffer |
| PI | Per event after exception reporting | 1 per collective member | Buffer queue |
| Sparkplug | Per metric, protobuf | 1 to the broker | Edge store if primary host offline |
| Synnax | Series payload | N streamers through one tap per node | Acquisition stops |

### 4.4 Configuration reach

| Shape | Unit of config | Fleet plan and diff | Cut-off site can change its own config |
|---|---|---|---|
| Foundation | Branch of the name tree, Raft per branch (K5, r4) | Yes (K3) | Yes |
| Zenoh | Process (JSON5) | No | Yes (local files) |
| DDS | Application (XML) | No | Yes |
| Broker plus agents | Broker objects (Terraform), agent files | Partial (broker side only) | Agents yes; broker objects no |
| PI | Primary archive tables; adapters via REST | No | No (primary only) |
| Ignition 8.3 | Gateway files plus EAM push | No | Yes on the gateway |
| UMH Core | `config.yaml` per instance | No | Yes |
| Synnax | Core metadata | No | No |

Reading: no alternative has a fleet-wide plan. Foundation's control plane is the
heaviest in this table; it buys the only "plan the whole fleet, and still let a cut-off
site change its own branch".

### 4.5 P1 evidence

| Target | Best evidence found | Mark |
|---|---|---|
| 100 M samples/s per node with disk buffer and one complete reader | Synnax 95.1 M at 200 to 1,000 channels on an M2 Max | VENDOR |
| | Zenoh 11.4 M msg/s at 8 B; 30 Gbit/s at 1 KiB over TCP (no storage) | M |
| | Canary 2.8 to 3.6 M TVQ/s; PI 25 to 80 k events/s per topology | VENDOR |
| Within 2x at 100 k channels | Synnax halves by 10 k channels | VENDOR |
| p99 < 250 us, one encrypted LAN hop | Zenoh TLS loopback p99 44 to 59 us | M |
| | Zenoh 16 us average between machines (TCP, unencrypted) | V1, project |
| | Cyclone DDS about 30 us | V1, project |
| | Zenoh QUIC loopback p99 74 to 170 us | M |
| < 4 bytes per sample | No alternative publishes this; PI and Sparkplug cut counts by exception instead | U |
| Pi 4 idle < 50 MB | zenohd 9.5 MB; nats-server 17 MB; Synnax 74 MB; Ignition Edge 2 GB minimum | M, M, M, U |

Reading: per-sample and per-message designs (PI, Canary, MQTT, DDS, Zenoh per message)
sit one to three orders of magnitude below P1's throughput. Only designs that move
series (Foundation, Synnax, Kafka batches, Zenoh with large payloads) can reach it.
Latency is reachable on TCP and TLS; QUIC is the open risk.

### 4.6 Score justification

Scale: 0 none, 1 weak, 2 partial, 3 strong.

| Shape | Transport | Connectors and time | Outbound | Agent | Mesh as code | P1 | Maturity |
|---|---|---|---|---|---|---|---|
| Foundation | 3: NAT, relays, multipath, TCP fallback (design) | 3: in-process connectors, error-bounded mesh clock (design) | 2: planned set, every client built new (r7) | 3: one operation table drives CLI, MCP, docs (C7) | 3: plan, apply, explain, export, branches | 2: series model fits; QUIC, codecs, 100 k channels unproven | 0 |
| Zenoh | 3: many links, routing, priorities, ACL; no hole punching found | 1: no industrial connectors; HLC orders, does not sync | 1: storages (InfluxDB 1.x full, 2.x partial), MQTT and DDS bridges | 1: admin space and REST; no MCP | 1: JSON5 per process | 2: fast transport, no buffer, per message | 3 |
| DDS | 2: LAN excellent; WAN needs vendor add-ons; multicast discovery | 1: none industrial; source timestamps | 1: vendor adapters | 1: XML and vendor GUIs | 1: XML per app | 2: latency great; small-sample rate 1 to 2 M/s | 3 |
| Broker plus agents | 1: client to server; NATS leaf nodes are the exception | 1: Telegraf OPC UA and Modbus inputs; no sync | 3: largest ecosystem | 2: CLIs, Terraform, MCP servers | 2: broker side only | 1: cluster fast, but Kafka and Redpanda miss the Pi; two durability domains | 3 |
| Historians | 1: proprietary client to server | 2: widest catalogs; PI offset correction | 2: broad but closed | 1: GUI first; Ignition REST and MCP early access | 1: Ignition 8.3 files per gateway | 0: 10^4 to 10^6 events/s; Windows or JVM | 3 |
| MQTT, Sparkplug, UNS | 2: hub with bridges, TLS, outbound only | 1: via third-party gateways; ms timestamps | 2: broker extensions | 1 | 1 | 1: 25 to 45 k msg/s per node in vendor cluster runs | 3 |
| Synnax | 1: client to server, no store and forward | 2: NI, LabJack, OPC UA, Modbus, EtherCAT; PC clock | 0 | 1: GUI first | 1 | 2: near P1 on one node; degrades with channels; 74 MB idle | 2 |
| Aeron | 2: fastest UDP and IPC; no NAT; encryption commercial | 0 | 0 | 1 | 1 | 3 for latency; JVM first | 3 |
| Pipelines | 1: sources and sinks, no mesh | 1: UMH adds industrial inputs; no sync | 3 | 2: MCP server (Redpanda Connect) | 2: per node | 1: tens of MiB/s per node (vendor) | 2 |

---

## 5. Is Foundation's shape the right base?

**Yes.** Four properties decide it, and each is missing elsewhere:

1. **Durable at the source, held per reader.** Only PI does this, with Windows,
   per-point events, and a closed stack. Brokers hold per reader only after the weak
   link. Zenoh, DDS, and Sparkplug hold nothing durable per reader by default.
2. **One control gate.** Only Synnax arbitrates at one point. DDS arbitrates per
   reader. Nobody else arbitrates.
3. **Time with an error bound.** Nobody else offers it. PI corrects offsets between
   archives; Zenoh orders with HLC; the rest trust the source clock.
4. **Fleet plan and apply that survives a cut-off site.** Nobody else offers it.

The series data model is also required: section 4.5 shows per-sample designs cannot
reach P1.

The shape is not new. It is Kafka's partition leader (ordering, numbering, positions)
moved to the edge, PI's source buffering generalized to many readers, DDS's ownership
and liveliness centralized into one gate, NATS's subject and account model, Terraform's
plan, and Synnax's channel and index model. That lineage is a strength: every element
has a field precedent.

What the shape still needs: copies near readers (decision 3), the failover contract
(r13), quarantine (decision 5), death records (decision 6), and lossy reduction for
weak links (decision 7).

---

## 6. Where Foundation's shape is weakest

1. **Novelty concentration.** Own transport with relays and NAT traversal (r5), own
   Raft (r4), own prolly-tree spec with branch delegation and epoch fencing (r4), own
   WAL and segment format and codecs (r2), own Modbus, MQTT, Sparkplug, and Kafka
   clients (r7), a thread-per-core runtime (r1), and a whole-mesh simulator (T1). Each
   choice has a good reason. Together they make the largest risk in this study. Every
   alternative stands on one mature core and grows around it. Fix: order the phases so
   the first end-to-end slice uses the fewest new parts (decision 9).
2. **WAN fan-out amplification.** With the home on the edge node and positions at the
   home, five cloud sinks mean five streams and five holds across Starlink. Every hub
   shape sends once. Fix: read copies placed by policy (decision 3). A1 already says
   "reads served from any copy"; nothing yet creates a copy except a standby.
3. **The QUIC data path.** Section 2 and r5 both point the same way: userspace QUIC
   costs CPU per byte and per packet, and macOS and Windows lack reliable offloads.
   P1's 3.2 Gbit/s to one remote complete reader is within reach on Linux with GSO, but
   unproven. Fix: measure first; keep TLS over kernel TCP as a first-class data path
   (decision 4).
4. **One home per index without a designed failover.** BQ6 to BQ10 are paused for r13.
   Edge reality softens this: if the DAQ node dies, acquisition stops in every shape.
   The risk is for remote writers (SDKs writing to a cloud home), control continuity,
   and disk loss. Fix: r13, plus a stated contract (Ignition says "not lossless"; say
   exactly what Foundation may lose).
5. **Operational paths the incumbents learned the hard way.** No quarantine: one frame
   that a sink rejects forever holds a durable reader and its buffer until the budget
   evicts data. No death record: a node's status channels live on that node, so when it
   dies, readers see silence, not "dead". No deadband or swinging-door reduction for
   slow links.
6. **Control plane weight.** Raft per branch, delegation records, epochs, forced
   takeover, and a content-addressed tree are more machinery than any alternative uses
   for config. PI uses one primary, Ignition one master or EAM controller, NATS files
   per server. Foundation's extra weight buys cut-off sites and fleet planning; it must
   be verified hard (etcd scenarios, TLA+ trace validation, r4) and kept out of the
   data path.
7. **Ecosystem gravity.** Zero users against Zenoh's ROS 2 Tier 1 status, Kafka, MQTT,
   and PI. Adoption depends on interop connectors (MQTT Sparkplug, Kafka, OPC UA, and
   later Zenoh and DDS).

---

## 7. Would building on Zenoh beat building our own?

**No, for the core. Yes, as a connector later.**

What Zenoh would replace, mapped to the r8 and BQ crate map:

| Foundation crate | Zenoh replaces it? |
|---|---|
| `transport` | Mostly: links, TLS, QUIC, priorities, batching, fragmentation |
| `hub` routing to remote homes | Partly: key-expression routing, but Foundation still routes by home |
| `blob` peer fetch | No (queryables could carry it, with new code) |
| `home`, `control`, `delivery`, `buffer` | No: no ordered log, no positions, no gate |
| `mesh`, `raft`, `spec` | No: no consensus, no fleet config |
| `time` | No: HLC orders, does not sync |
| `codec`, `wire` | No: Zenoh payloads are opaque bytes |
| `connector-*`, `config`, `ops`, `sim` | No |

So Zenoh would replace one crate and part of another, out of about twenty.

What it would cost:

- **T1 deterministic simulation.** Zenoh runs its own pools of Tokio runtimes
  (Application, Acceptor, TX, RX, Net; docs.rs `zenoh-runtime`) [V1] and its timers read
  the runtime's clock. The real transport would never run under the simulator. This is
  the same reason r5 rejected iroh's `Endpoint` [U: inference from the runtime design;
  no Zenoh simulation mode found].
- **C2 shard model.** Zenoh's own thread pools sit beside Foundation's shards; every
  frame crosses threads on the way in and out. r1 measured that handoffs, not shards,
  carry the tail risk.
- **Two routing models.** Zenoh routes by key expression and interest; Foundation
  routes by home and selector. Debugging would cross both.
- **Delivery semantics.** Zenoh's hop reliability, drop-after-1-ms, and
  close-after-5-s blocking sit under Foundation's end-to-end seq and credits. Foundation
  would configure around them, not use them.
- **Wire versioning.** C9d gates formats by mesh finalization. Zenoh's wire format is
  ZettaScale's to change.
- **NAT traversal.** Not found in Zenoh; Foundation needs relays and hole punching
  anyway (r5) [U].

What it would give: a proven transport, fast today (section 2), shared memory, a
microcontroller client, and interop with ROS 2. The first two matter most if the
in-house transport slips. r5 already holds a hedge for that (prototype on iroh behind
the `Transport` trait, switch before the first stable).

**Zenoh as a competitor.** Zenoh overlaps on "Rust, one binary, edge to cloud, runs on a
Pi" and leads on maturity. It lacks a durable per-reader log, control arbitration, time
bounds, industrial connectors, sinks beyond storages, and fleet config. ZettaScale
targets robotics and automotive. A user can assemble a partial Foundation from Zenoh
plus storages plus custom bridges today. Foundation's moat is the semantics in section
5, not the transport. The RFC should say so (decision 11).

**Other "build on" options**, briefly:

- **NATS JetStream**: closest whole shape; Go, so not one Rust binary; per-message
  overhead; Jepsen's fsync findings; no gate, time, or connectors. No.
- **DDS (Cyclone, compiled in)**: discovery and WAN problems, durability as a separate
  service. No.
- **Kafka or Redpanda**: footprint misses the Pi by 40x to 80x. No.
- **iroh**: r5 decided: noq-proto core, own driver; iroh only as a prototype hedge.

---

## 8. What to take from each alternative

| From | Take | Where it lands |
|---|---|---|
| Zenoh | Multi-link with TLS over kernel TCP as the fast default | Decision 4, r5 |
| Zenoh | Subscription aggregation at a node | Decision 3, `hub` |
| Zenoh | `express` per writer; per-hop timestamp instrumentation | B6, C7 |
| Zenoh | No silent drops; every drop is a counted gap | B5 |
| Zenoh | Shared-memory local SDK path; microcontroller client | Later |
| DDS | QoS vocabulary mapping in the RFC | Decision 11 |
| DDS | Plan-time compatibility checks | Decision 8 |
| DDS | DEADLINE as silence detection per connector group | Decision 6 |
| Brokers | Send once across the weak link, fan out near readers | Decision 3 |
| Brokers | Fsync before any durable ack; publish benchmark setups | r13, P1 |
| NATS | Small whole-shape proof: leaf nodes, accounts, subjects, Raft log in 17 MB | Decision 10 |
| PI | n-way buffering to each copy on its own queue | Decision 3, r13 |
| PI | Exception and swinging-door reduction | Decision 7 |
| PI | Periodic offset correction between nodes | C6, confirmed |
| Ignition | Quarantine | Decision 5 |
| Ignition | Honest failover contract; deployment modes | r13, K2 |
| Sparkplug | Death certificates | Decision 6 |
| Sparkplug | Aliases after birth; resume by seq, never rebirth | A4, A8 confirmed |
| Synnax | Tapper aggregation; channel and index model; authority | Decision 3, A7, S11 |
| Aeron | Archive by position; Cluster log plus deterministic service | r13 |
| Aeron | Back-pressure as a return value, never a block | B5 confirmed |
| Pipelines | MCP over pipelines; live `tap` for agents | C7 |
| UMH Core | Market proof for "one box, UNS, buffer, text config" | Positioning |

---

## 9. Decisions for the user

1. **Keep the shape.** Keep the mesh of identical nodes with one home per index, the
   home's own durable buffer with reader positions, Raft-governed spec branches, and
   in-process connectors as Foundation's base.
   **Recommendation: yes.** No alternative gives durable-at-source with per-reader
   holds, one control gate, time bounds, and fleet plan and apply together (section 5).

2. **Do not build on Zenoh; add a Zenoh connector later.** Build the core on
   Foundation's own crates as planned. Add `connector-zenoh` (in and out) after the
   first wave for ROS 2 and Zenoh interop.
   **Recommendation: yes.** Zenoh would replace about one crate of twenty and break T1
   and C2 (section 7). Its users are reachable through a connector.

3. **Read copies near readers.** Add a placement setting that keeps a read copy of an
   index on a named node, for example `[[placement]] select = "site_a.**" copies =
   ["cloud.gw_1"]`. The copy is a complete reader with a hold at the home (the same
   mechanism Q6 proposes for standbys). Remote durable readers hold at the copy, so
   the weak link carries each sample once and holds once. `hub` also merges latest
   subscriptions for the same remote home into one upstream flow.
   **Recommendation: yes.** It fixes WAN fan-out (section 6, item 2) with existing
   concepts: a policy, a selector, a reader with a hold. Precedents: JetStream mirrors,
   Kafka MirrorMaker, PI n-way buffering, Zenoh routers, the Synnax relay tapper.

4. **Measure QUIC against TLS over TCP on Linux before locking the transport.** Run
   the comparison as the first transport benchmark on the Linux HITL runner: one
   connection per shard, GSO on, 64 B to 64 KiB, p50 to p99.9 latency and Gbit/s.
   **Recommendation: measure first; default plan if QUIC misses P1: TLS over kernel
   TCP streams for LAN data, QUIC for WAN, NAT traversal, and multipath.** Section 2
   measured Zenoh's QUIC at 0.7 Gbit/s against 13 to 19 Gbit/s for its TLS link on
   macOS. This revises r5 only if Linux confirms the gap.

5. **Quarantine for data a sink rejects.** After N failures on the same frame, an out
   connector records the range and the error on its status channel, skips it, and
   moves on. `foundation` shows quarantined ranges and can replay them as backfill.
   **Recommendation: yes, off by default per connector kind only where the sink can
   never reject a frame.** Without it, one bad frame holds a reader and its buffer until
   the budget evicts data (B1). Precedents: Ignition quarantine, Kafka Connect dead
   letter queues.

6. **Death and silence records written by the mesh.** When a node's lease lapses, the
   voters of its branch write that on a mesh-owned channel (for example
   `<node>.alive`). When a connector group sees no sample for k times its expected
   interval, its node writes that on the group's status channel.
   **Recommendation: yes.** Today a dead node's status channels go silent, and readers
   cannot tell dead from quiet. Precedents: Sparkplug NDEATH through the MQTT will,
   DDS LIVELINESS and DEADLINE.

7. **Lossy reduction functions for weak links.** Add deadband (exception) and
   swinging-door compression as calculation functions (A17), opt-in, shown as lossy in
   `plan`.
   **Recommendation: yes, never as a default.** PI's two-stage reduction is the most
   proven data reduction in this industry; one study measured 80% fewer values. It
   lets a Starlink site send a fraction of the samples without changing acquisition.

8. **Plan-time compatibility checks.** `plan` rejects setting pairs that cannot work:
   an out connector with a hold on an index with no retention, a command group without
   a max age, a latest reader max age shorter than the index's interval, a read copy
   in a branch the reader cannot access.
   **Recommendation: yes.** DDS's worst failure is silent QoS mismatch. Foundation has
   every setting in the spec, so it can catch these before apply.

9. **Cut the first phase's novelty.** The first end-to-end slice: one node, TLS over
   TCP, a static spec with no Raft, one connector, one sink, the disk buffer, under
   DST, measured against P1. Then add QUIC and relays, Raft and branches, read copies.
   **Recommendation: yes.** It measures P1 early and keeps the riskiest new parts off
   the first critical path (section 6, item 1).

10. **Idle footprint gate from phase one.** Add idle RSS < 50 MB and binary size to the
    P1 merge gate. Roles (voter, relay, time source, read copy) allocate nothing until
    assigned.
    **Recommendation: yes.** zenohd idles at 9.5 MB and nats-server at 17 MB; the
    Synnax Core idles at 74 MB [M]. Footprint grows silently unless a gate stops it.

11. **An alternatives section in the RFC.** A short section built from this report:
    why not Zenoh, NATS, DDS, PI, or MQTT; the DDS and Sparkplug vocabulary mapping;
    the four properties no alternative has.
    **Recommendation: yes.** It answers the first question every evaluator will ask
    and records why the shape was chosen.

---

## Sources

Zenoh:
- Zenoh vs MQTT, Kafka, DDS (2023): https://zenoh.io/blog/2023-03-21-zenoh-vs-mqtt-kafka-dds/
  and https://arxiv.org/abs/2303.09419
- Zenoh 1.5.0 (Hong): https://zenoh.io/blog/2025-07-28-zenoh-hong/
- Zenoh 1.10: https://zenoh.io/blog/2026-08-17-zenoh-1.10/
- Abstractions: https://zenoh.io/docs/manual/abstractions/
- Default config at 1.10.1: https://raw.githubusercontent.com/eclipse-zenoh/zenoh/1.10.1/DEFAULT_CONFIG.json5
- Reliability: https://zenoh.io/blog/2021-06-14-zenoh-reliability/
- Storage alignment: https://zenoh.io/blog/2022-11-29-zenoh-alignment/
- Access control: https://zenoh.io/docs/manual/access-control/
- zenoh-ext: https://docs.rs/crate/zenoh-ext
- zenoh-runtime: https://docs.rs/zenoh-runtime
- uhlc: https://docs.rs/crate/uhlc/latest
- Plugins: https://zenoh.io/docs/manual/plugins/
- Spec 1.0.0: https://spec.zenoh.io/spec/1.0.0/index.html
- InfluxDB backend: https://github.com/eclipse-zenoh/zenoh-backend-influxdb
- Zenoh-Pico performance: https://zenoh.io/blog/2025-04-09-zenoh-pico-performance/
- rmw_zenoh and ROS 2 Kilted: https://docs.ros.org/en/kilted/Releases/Release-Kilted-Kaiju.html,
  https://www.openrobotics.org/blog/2025/5/23/ros-2-kilted-kaiju-released,
  https://roscon.ros.org/2024/talks/RMW_Zenoh-_An_alternative_middleware_for_ROS_2.pdf,
  https://docs.stereolabs.com/docs/integrations/ros-2/using-zenoh-as-middleware

DDS:
- Cyclone DDS README: https://github.com/eclipse-cyclonedds/cyclonedds
- Systematic Analysis of DDS Implementations (Middleware 2023):
  https://www.ce.cit.tum.de/caps/aktuelles/news-single-view/article/accepted-paper-at-middleware-2023-systematic-analysis-of-dds-implementations/
- RTI ownership and durability QoS: https://community.rti.com/static/documentation/connext-dds/7.3.1/doc/api/connext_dds/api_ada/DDSOwnershipQosModule.html,
  https://community.rti.com/static/documentation/connext-dds/7.1.0/doc/api/connext_dds/api_cpp/structDDS__DurabilityQosPolicy.html
- RTI incompatible QoS status: https://community.rti.com/rti-doc/510/ndds/doc/html/api_cpp/structDDS__OfferedIncompatibleQosStatus.html
- RTI WAN transport and Cloud Discovery Service NAT traversal:
  https://community.rti.com/static/documentation/connext-dds/7.3.0/doc/manuals/addon_products/cloud_discovery_service/nat.html
- RTI commercial license: https://community.rti.com/node/2461
- Fast DDS license: https://index.ros.org/p/fastdds/

Brokers and agents:
- Vanlightly, Kafka vs Redpanda: https://jack-vanlightly.com/blog/2023/5/15/kafka-vs-redpanda-performance-do-the-claims-add-up
- Jepsen NATS 2.12.1: https://jepsen.io/analyses/nats-2.12.1; Synadia response:
  https://www.synadia.com/blog/jepsen-nats-2-12-1
- NATS leaf nodes and JetStream: https://docs.nats.io/learn/topologies/leaf-nodes,
  https://docs.nats.io/running-a-nats-service/configuration/leafnodes/jetstream_leafnodes
- nats bench: https://docs.nats.io/using-nats/nats-tools/nats_cli/natsbench
- NATS and CNCF: https://www.cncf.io/blog/2025/04/24/protecting-nats-and-the-integrity-of-open-source-cncfs-commitment-to-the-community
- Redpanda requirements and license: https://docs.redpanda.com/current/deploy/redpanda/manual/production/requirements/,
  https://docs.redpanda.com/streaming/24.2/get-started/licenses/
- Confluent KRaft sizing: https://docs.confluent.io/platform/current/kafka-metadata/config-kraft.html
- Vector buffering: https://vector.dev/docs/architecture/buffering-model/; license:
  https://vector.dev/highlights/2020-08-31-mpl-2-0-license/
- OTel Collector resiliency: https://opentelemetry.io/docs/collector/resiliency/
- Telegraf buffers: https://oneuptime.com/blog/post/2026-08-24-size-monitor-telegraf-buffers/view
- Kafka Connect KIP-618: https://cwiki.apache.org/confluence/display/KAFKA/KIP-618:+Exactly-Once+Support+for+Source+Connectors
- Redpanda Connect licensing and MCP: https://docs.redpanda.com/connect/get-started/licensing/,
  https://docs.redpanda.com/streaming/current/reference/rpk/rpk-connect/rpk-connect-mcp-server/
- Bento fork: https://www.warpstream.com/blog/announcing-bento-the-open-source-fork-of-the-project-formerly-known-as-benthos
- UMH Core: https://docs.umh.app/umh-core-vs-classic-faq

Historians:
- PI buffering and collectives: https://docs.aveva.com/bundle/pi-server-s-buf-ha/page/1020154.html,
  https://docs.aveva.com/bundle/pi-server-s-buf-ha/page/1019485.html
- PI to PI time offset: https://docs.aveva.com/bundle/pi-to-pi-interface/page/1012178.html
- PI topologies: https://docs.aveva.com/bundle/pi-server-l-topologies/page/1271936.html
- Exception and compression study (AIChE 2023):
  https://aiche.org/conferences/aiche-annual-meeting/2023/proceeding/paper/314f-statistical-evaluation-data-exception-and-compression-algorithm-applied-industrial-data
- PI adapters REST config: https://docs.aveva.com/bundle/adapter-for-mqtt/page/1230892.html
- Ignition store and forward: https://docs.inductiveautomation.com/docs/8.3/platform/store-and-forward
- Ignition redundancy: https://docs.inductiveautomation.com/docs/8.3/platform/ignition-redundancy/setting-up-redundancy,
  https://docs.inductiveautomation.com/docs/7.9/gateway/ignition-redundancy
- Ignition 8.3 version control and deployment modes: https://www.docs.inductiveautomation.com/docs/8.3/tutorials/version-control-guide,
  https://inductiveautomation.com/blog/node/4487
- Ignition EAM: https://docs.inductiveautomation.com/docs/8.3/ignition-modules/enterprise-administration
- Ignition MCP module: https://forum.inductiveautomation.com/t/ignition-mcp-server-when-and-what/111523
- Ignition Edge on ARM: https://industrialmonitordirect.com/blogs/knowledgebase/ignition-maker-81-raspberry-pi-4-installation-guide
- Canary performance: https://blog.canarylabs.com/2016/06/27/data-historian-performance-test-pushing-canary-to-the-limit
- Canary licensing and ports: https://helpcenter.canarylabs.com/t/g9hj7l1/how-to-license-canary-system-components-version-22,
  https://helpcenter.canarylabs.com/t/p8y8g15/canary-endpointsports-version-24

MQTT, Sparkplug, UNS:
- Sparkplug 3.0 specification (Eclipse; local copy `scratchpad/sp.txt`, Primary Host
  and `is_historical` sections)
- EMQX 100 M connections: https://dev.to/emqx/reaching-100m-mqtt-connections-with-emqx-50-4lki
- EMQX license: https://www.emqx.com/en/content/license-faq
- HiveMQ 200 M connections: https://hivemq.com/benchmark-10-million
- HiveMQ Edge features: https://www.hivemq.com/blog/hivemq-edge-2024-3-released
- UNS limits: https://www.synadia.com/blog/what-a-unified-namespace-requires,
  https://iiot-world.com/smart-manufacturing/understanding-unified-namespace-seven-key-questions-answered/

Other:
- Aeron: https://highscalability.com/aeron-do-we-really-need-another-messaging-system/,
  https://docs.stacresearch.com/system/files/resource/files/STAC-Summit-14-May-2024-Adaptive.pdf,
  https://weareadaptive.com/aeron
- Arrow Flight benchmark: https://arxiv.org/abs/2204.03032
- Vector vs Fluent Bit harness summary: https://adhdecode.com/articles/vector/vector-benchmarks-vs-fluentbit-logstash/
- OPC UA PubSub: https://opcfoundation.org/?p=3968, https://arxiv.org/pdf/2310.17052

Synnax (this repo):
- `docs/claude/architecture.md`
- `core/pkg/distribution/channel/channel.go:26-49`
- `core/pkg/distribution/framer/relay/README.md`
- `driver/task/manager.cpp:170-196`
- `driver/control/state.h:135-145`
- `x/go/control/authority.go:23-24`
- `aspen/README.md:34-48`
- `site/docs/src/pages/blog/one-billion-rows/index.mdx`
- `LICENSE`

Foundation inputs: `memory/project_foundation_design.md`; `foundation-research/`
r1 (thread model), r2 (storage), r4 (consensus), r5 (transport), r6 (time), r8
(boundaries), and the research ledger.
