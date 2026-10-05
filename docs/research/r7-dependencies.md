# Foundation research fork 7: build-or-adopt audit of major dependencies

Date: 2026-10-04. Scope: areas 1-9 from the directive. The runtime, consensus, gossip,
transport, and time sync belong to forks 1, 4, 5, and 6 and are not covered here.

Method: architectural need first, then candidates. Maturity data comes from the
crates.io API and the GitHub API, pulled on 2026-10-04. Each load-bearing claim has two
sources, or it is marked **UNVERIFIED**. Binary size and idle memory were **not
measured**: that needs one build per candidate, which belongs in fork 1's benchmark
harness. The size notes below are qualitative and marked as such.

Legend for the DST column (T1 injection rule):
- **sans-I/O**: the code does no I/O and reads no clock, so it runs unchanged in the
  simulator.
- **seam**: the library does its own I/O or reads the clock, but it can sit behind a
  Foundation trait. The simulator then uses a fake on our side of the seam.
- **none**: no seam is possible inside the library, so it stays out of simulation and
  is tested with protocol simulators and real devices (T1 layers 7 and 8).

## Summary

| # | Area | Verdict | Pick | License | DST | C code? |
|---|---|---|---|---|---|---|
| 1 | OPC UA client and server | **Adopt + fork crypto** | async-opcua 0.19.0, crypto moved to aws-lc-rs | MPL-2.0 (file-level, OK) | Client: seam (has `Transport` trait). Server: none. | No |
| 2 | Modbus TCP and RTU | **Build** | Own sans-I/O codec + drivers; serial2 for serial ports | Ours; serial2 BSD-2/Apache | sans-I/O | No |
| 3 | MQTT and Sparkplug B | **Build** | Own sans-I/O MQTT 3.1.1/5 client + Sparkplug state machine | Ours; schema source is a decision | sans-I/O | No |
| 4 | Kafka | **Build** (recommended) or wrap | Own client on generated protocol codecs (`kafka-protocol` 0.18 or our own generator) | Ours; kafka-protocol MIT/Apache | sans-I/O | No (wrap option: yes, librdkafka) |
| 5 | NI DAQmx, LabJack LJM | **Build** | Own runtime-loaded bindings (libloading); LabJack Ethernet over our own Modbus + stream | Ours; vendor runtimes not redistributed | seam (fake device layer) | Vendor libs at runtime only |
| 6 | Compression codecs | **Build** | Own ALP, FastLanes-style bitpacking/FOR/delta, timestamp stride; spiraldb `alp`/`fastlanes` and `pco` as test oracles | Ours; oracles Apache-2.0 | Pure functions | No |
| 7 | Crypto and TLS | **Adopt** | rustls + aws-lc-rs as the only provider; blake3 | Apache/ISC/MIT; aws-lc ISC/Apache | rustls is sans-I/O | Yes (aws-lc, required) |
| 8 | CLI, MCP, schema, config, metrics | **Adopt** clap, schemars, toml/toml_edit, tracing; **Build** thin MCP server and Prometheus encoder | see notes | MIT/Apache | Pure, or seam | No |
| 9 | InfluxDB, Ignition, Grafana | **Build** (small) | Own line-protocol writer on one HTTP client; Ignition via our OPC UA server and Sparkplug; Grafana via stores and Live push | Ours | seam (HTTP client) | No |

C code that would ship in the binary: aws-lc (required by area 7). Under the Kafka
"wrap" option, librdkafka would also ship. Vendor DAQmx and LJM libraries are loaded at
runtime and are never linked or redistributed.

## 1. OPC UA client and server

**Need.** We need a client for PLCs and OPC UA servers, with subscriptions, browse (for
`discover`), read, write, and history read. We need a server that exposes channels to
Ignition and other OPC UA clients. Security must cover today's RSA policies
(Basic256Sha256, Aes128_Sha256_RsaOaep, Aes256_Sha256_RsaPss) and, eventually, the
OPC UA 1.05 ECC policies. The crypto must be constant time.

**Candidates.**
- **async-opcua 0.19.0** (pure Rust, Tokio, client + server). MPL-2.0.
  - Releases: 0.19.0 on 2026-07-18, 0.18.0 on 2026-02-24, 0.17 in 2025-12, and 0.16 in
    2025-07. That's a release every 2 to 5 months. 156 stars, 9 open issues, and the
    last push was 2026-10-02.
  - Contributors: locka99 (the original author, 2,085 commits) and einarmo (217
    commits, the active maintainer). einarmo's employer is **UNVERIFIED**.
  - Size: about 105k hand-written lines plus about 329k generated lines (types and the
    standard address space).
  - There's a `dotnet-tests` directory that tests against the OPC Foundation .NET
    stack.
  - TODO.md lists some missing features: encrypted secrets (Part 4 7.41.2.3) and the
    Query service.
- **open62541** (C99, MPL-2.0). Fraunhofer IOSB maintains it. A server built on v1.0
  was certified for the Micro Embedded Device Server profile in 2019. Version 1.5
  supports ECC policies through OpenSSL 3 or mbedTLS 3. The `open62541` Rust crate
  0.12.0 (HMIProject, MPL-2.0) wraps it for both client and server. Its EventLoop
  plugin exposes `dateTime_now` and `dateTime_nowMonotonic` plus custom
  ConnectionManagers, so time and network could in principle be injected.
- **Build our own.** The protocol is large: binary encoding (generated from the XML
  schemas), the secure channel, sessions, subscriptions, the address space, and every
  security policy. The OPC Foundation's conformance tool (CTT) is members-only.
  async-opcua's 105k hand-written lines show the scale.

**Crypto gap (verified).** async-opcua's `async-opcua-crypto` crate depends on RustCrypto
`rsa`, `aes`, `cbc`, `sha1`, and `sha2`, and it has no ECC (checked in its
`Cargo.toml`). RUSTSEC-2023-0071 (the Marvin attack: RSA private key recovery through
timing) is still open with no patched version (osv.dev, rustsec.org). An OPC UA server
decrypts RSA traffic from the network, which is exactly the exposure the advisory warns
about.

**DST.** The client has `Connector`/`Transport` traits
(`async-opcua-client/src/transport/connect.rs:19,100`), so a transport can be plugged
in. The server uses `TcpListener` directly (`async-opcua-server/src/server.rs`). There
are 88 direct clock reads across client, server, and core. So OPC UA stays outside
deterministic simulation. Our connector seam fakes it there, and the real stack is
tested against protocol simulators (open62541 server, .NET reference server) in T1
layer 7.

**Size (qualitative, UNVERIFIED).** The server's generated standard namespace is the
largest single contributor. Measure the size with the namespace feature on and off.

**Verdict: adopt async-opcua, and fork only its crypto crate onto aws-lc-rs.** aws-lc
gives us constant-time RSA and ECDSA and puts every crypto call in Foundation on one
provider (area 7). Offer the change upstream. ECC policies come later in the same
crypto layer. Brainpool support in aws-lc is **UNVERIFIED**. Keep open62541 as the
interop test peer, not as a dependency, because FFI to C would bring C into the
protocol path. Building our own stack isn't worth it: the cost is very high, and the
only gap is the crypto layer, which we can replace.

## 2. Modbus TCP and RTU

**Need.** Our own reads and writes, chunked by register ranges, typed swaps per device
(a Synnax lesson: Modbus swap fields were never read), RTU over RS-485, and Modbus TCP
for PLCs and LabJack T-series devices (area 5). One device actor owns each handle.

**Candidates.**
- **tokio-modbus 0.17.0** (slowtec, MIT/Apache, 2025-10-22, 568 stars, 44 issues). It
  does its own Tokio I/O.
- **rmodbus 0.12.2** (alttch, Apache-2.0). Its docs describe it as a transport-
  independent `no_std` frame codec that ships no server. Last push 2025-09-29.
- **Build our own.** Modbus framing (MBAP header, PDU, RTU CRC, inter-frame timing) is
  small and fully specified.

**Verdict: build our own sans-I/O Modbus codec and client/server state machines** in
the Modbus connector crate. It runs unchanged in the simulator, and the Modbus protocol
simulator for T1 layer 7 reuses the same codec as its server side. rmodbus is the
reference for differential tests.
- Serial I/O: **serial2 0.2.38** (BSD-2-Clause or Apache-2.0, last push 2026-07-31,
  concurrent reads and writes even on Windows). It beats serialport 4.10.1 (MPL-2.0,
  106 open issues). RS-485 direction control on Linux through serial2 is
  **UNVERIFIED**, so check it before locking.

## 3. MQTT client and Sparkplug B

**Need.** An MQTT 3.1.1 and 5 client with TLS, QoS 0/1/2, a Will message, and
keepalive. On top of it, Sparkplug B 3.0 as both edge node and host: births, deaths,
`bdSeq`, seq 0-255, rebirth, STATE, and `is_historical` for backfill.

**Candidates.**
- **rumqttc 0.25.1** (Bytebeam, Apache-2.0). Last release 2025-11-21, last push
  2026-05-01, 183 open issues. Activity is moderate (depscope rates maintenance 15/25).
- **rumqttc-next 0.34.0**, a fork created 2026-02-16 with 27 stars and one maintainer.
- **ntex-mqtt 9.0.0** runs on the ntex runtime, not Tokio, so it doesn't fit.
- **paho-mqtt 0.14.0** wraps the Paho C library under EPL-2.0.
- **Sparkplug crates**: srad 0.5.0 (Apache-2.0, 10 stars, one author) and sparkplug-rs
  0.5.1 (EPL-2.0).
- **Build our own.** MQTT is small and fully specified. A sans-I/O codec plus a session
  state machine (packet ids, in-flight QoS flows, keepalive on the injected clock) is a
  bounded job.

**Sparkplug licensing and patents (verified).**
- The Tahu `sparkplug_b.proto` and the spec repository are EPL-2.0 (GitHub API).
- The specification documents may be copied and used freely, with attribution in
  implementations (sparkplug.eclipse.org FAQ).
- **Cirrus Link holds patent US 11121930 B1** ("Sparkplug-aware MQTT Server"). Its
  royalty-free license covers only implementations that **pass the Sparkplug TCK** and
  comply with the TCK license (sparkplug.eclipse.org FAQ and specification page). So
  passing the TCK is a requirement for Foundation, not an option.
- EPL-2.0 is weak copyleft and allows commercial object code if the EPL-covered source
  stays available (eclipse.org EPL text). One community post calls the proto unusable
  in commercial code. That's an opinion, not a finding. The Synnax MQTT work (PR #2950)
  generates from the Tahu proto with a legal check still owed.

**Verdict: build our own sans-I/O MQTT client and Sparkplug B state machine.** Both run
in simulation, and Sparkplug's ordering and rebirth rules are exactly the kind of logic
the simulator should test. Interop: Mosquitto, EMQX, and HiveMQ brokers, plus the
Sparkplug TCK as a release gate. A broker isn't needed, since Foundation is a client.

## 4. Kafka

**Need.**
- **Out**: a producer to topics, with idempotence so that retries don't duplicate (B3
  is at least once).
- **In**: a consumer from topics.
- **Transactions**: only needed if we commit our position inside Kafka atomically with
  the records. Foundation keeps reader positions at the home (B2/S10), so transactions
  aren't required to get no-duplicate delivery. Idempotent writes plus per-sequence
  keys get us there.
- **Auth**: SASL/SCRAM, mTLS, and OAuth.

**Candidates.**
- **rdkafka 0.39.0** (MIT) over librdkafka 2.12 (C, BSD-2). This is the only Rust path
  with consumer groups and transactions today. Its C build (cmake) complicates
  cross-compiling to ARM and Windows. librdkafka runs its own threads and clock, so
  there's no DST seam inside it. It supports the KIP-848 consumer protocol only as
  early access since librdkafka 2.10 (Confluent blog).
- **rskafka 0.6.0** (InfluxData, MIT/Apache, 2025-03). Its README says: no offset
  tracking, no consumer groups, no transactions, and no longer used by InfluxDB 3.
- **kafka-protocol 0.18.0** (MIT/Apache, 2026-08-20, 7.4M downloads). It generates
  codecs for every Kafka message from Kafka's JSON message specs, and it does no I/O.
- **Build our own client** on those codecs.

**Key fact (verified).** KIP-848, the new consumer group protocol, became generally
available in Apache Kafka 4.0. It moves assignment logic from the client to the broker
(kafka.apache.org 4.0 docs and the Confluent blog). That makes a new client's consumer
group code much smaller than the old protocol required.

**Verdict: build our own pure-Rust client** (sans-I/O state machines over generated
codecs), covering the idempotent producer, consumers with manual partitions, and KIP-848
consumer groups. Transactions come only if a real case needs them. The alternative is
to wrap rdkafka behind a build flag, which ships fastest but brings C, threads, and
cross-compile pain, and stays outside simulation. This is a decision for the user (see
the end).

## 5. NI DAQmx and LabJack LJM

**Need.** The single binary must start on machines without vendor drivers, so vendor
libraries must load at runtime, never link at build time. One device actor owns each
handle (the Synnax LabJack handle race).

**Findings.**
- **ni-daqmx-sys 26.2.1** (WiresmithTech, MIT, 2026-06-26) **links at build time**:
  its `build.rs` emits `cargo:rustc-link-lib=nidaqmx` and refuses to build on 32-bit
  Unix. That rules it out. **daqmx 0.0.1** (published today) is an early API on top of
  it.
- NI's own `ni/grpc-device` repository (MIT) ships `imports/include/NIDAQmx.h`, but
  the header itself says "Copyright (c) National Instruments 2003-2026. All Rights
  Reserved". Vendoring the header needs a legal check (**UNVERIFIED** whether the
  repository's MIT license covers it).
- From earlier research: DAQmx runs on Linux x86-64 only (RHEL, openSUSE, Ubuntu
  22.04/24.04) and Windows, with no macOS and no ARM.
- LabJack: `ljm` 0.3.0 and `ljm-sys` link at build time. `ljmrs` 0.2.2 uses
  libloading but has no repository listed. `async_labjack` 0.1.0 (MIT, 4 stars) speaks
  Modbus TCP and LabJack's streaming directly, so it needs no LJM for Ethernet devices.
- Synnax also loads both vendor libraries at runtime
  (`driver/ni/daqmx/prod.cpp:22` uses `nicaiu.dll`; `driver/labjack/ljm/api.h:20`).
  That's the lesson, not code we'd reuse.

**Verdict: build our own.**
- **DAQmx**: a runtime-loaded function table, generated with bindgen's
  `--dynamic-loading` mode or written by hand for the functions we use, loaded with
  libloading 0.9.0 (ISC). The connector reports "driver not installed" through its
  status channel.
- **LabJack**: Ethernet devices use our own Modbus (area 2) plus LabJack's streaming
  protocol. USB devices use a runtime-loaded LJM table.
- **DST**: the seam is a device trait. The simulator uses a fake device, and real
  hardware runs on the HITL rigs (T1 layer 8).

## 6. Compression codecs

**Need.** Codecs for our own disk and wire format (S2, S4): fast decode,
format stability that we control (C9d's integer format versions), no per-series headers
on the wire, and pure functions.

**Candidates.**
- **fastlanes 0.7.2** and **alp 0.0.4** (spiraldb, Apache-2.0). Both are active (last push
  2026-10-05 UTC). alp is at 0.0.x. FastLanes decodes with auto-vectorized scalar code and
  no intrinsics (README). ALP and ALP-RD come from the SIGMOD 2024 paper by Afroozeh,
  Kuffo, and Boncz (CWI, DuckDB).
- **vortex-alp / vortex-fastlanes 0.87.0**: part of the Vortex file format and tied to
  it.
- **pco 1.0.3** (Apache-2.0). Its paper (arXiv 2502.06112) reports a 29-94% higher
  compression ratio than alternatives and decoding above 1 GiB/s per thread, and it's
  used by Zarr and CnosDB. pco is a full format with its own chunk metadata. Its formal
  format-stability guarantee is **UNVERIFIED**.
- **bitpacking 0.9.3** (Quickwit, MIT).
- **Build our own.** These algorithms are published and fairly small: ALP, FastLanes
  bitpacking with FOR and delta, and timestamp stride detection.

**Verdict: build our own codec implementations in `codec`.** The format is the product
(the user: "using our own layout is one of the key advantages"). Depending on a crate
for the byte format would tie our disk format to someone else's versioning. Use
spiraldb `alp`/`fastlanes` and `pco` as oracles in property and fuzz tests: decode their
output and compare ratios and speeds in `bench/`. Whether pco joins as an optional
high-ratio codec for slow links belongs to fork 2.

## 7. Crypto and TLS

**Need.** TLS 1.3 for QUIC, Ed25519 node and subject keys (S8, C8), X25519 sealed
secrets (K4), constant-time RSA and ECDSA for OPC UA, HTTPS for outbound connectors,
release signing (C9d), and BLAKE3 for content addressing (S9). An optional FIPS build.

**Candidates.**
- **rustls** (Apache/ISC/MIT, 0.23.x stable, 0.24 in development). It does no I/O: it
  moves TLS bytes in and out of buffers, so it fits the simulator. Its default provider
  is aws-lc-rs.
- **aws-lc-rs 1.18.1** (AWS, ISC + Apache). Its FIPS mode is covered by FIPS 140-3
  certificate #4816, with later releases certified or pending, per `aws-lc-fips-sys`
  (rustls FIPS manual).
  - A non-FIPS build needs a C compiler, plus NASM or prebuilt NASM objects on Windows
    x86-64.
  - A FIPS build also needs CMake and Go (aws-lc-rs requirements pages).
- **ring 0.17.14**. After its author paused, the rustls team took over maintenance
  (RUSTSEC-2025-0007, rustls maintainers' statements). iroh defaults to ring
  (earlier research).
- **Build our own: never.**

**Verdict: adopt rustls with aws-lc-rs as the only crypto provider in the binary.**
- Set iroh (fork 5) and async-opcua's crypto (area 1) to it, so every algorithm has one
  implementation.
- Add **blake3 1.8.7** for hashing (CC0-1.0 or Apache-2.0).
- A FIPS variant is a separate build. That's a decision for the user (see the end).

## 8. CLI, MCP, schema, config parsing, metrics

**Findings.**
- **clap 4.6.7** (MIT/Apache, 1.19B downloads). Adopt. C7's operation table drives
  clap's `Command` tree, completions, and man pages.
- **schemars 1.2.2** (MIT, JSON Schema 2020-12). Adopt. It produces the schemas for
  config (agent validation, C3) and for operations (C7).
- **toml 1.1.6 / toml_edit 0.25.15** (MIT/Apache, TOML 1.1). Adopt if K1 picks TOML
  (fork 3). toml_edit preserves formatting when `discover` writes into user files.
- **rmcp 3.5.0** (the official MCP SDK, Apache-2.0). It has had **20 incompatible
  release lines in 18 months**: 17 `0.x` minors from 2025-03-24 to 2026-02-27, then 1.0 on
  2026-06-23, 2.0 on 2026-07-08, and 3.0 on 2026-09-28 (crates.io version list).
- **tracing 0.1.44**: adopt for logs.
- **prometheus-client 0.25.1** (Apache/MIT) uses an injected registry (earlier
  research).

**Verdict.**
- **Adopt** clap, schemars, toml/toml_edit (pending K1), and tracing.
- **Build a thin MCP server.** It covers JSON-RPC, `initialize`, `tools/list`,
  `tools/call`, stdio, and streamable HTTP, generated from the C7 operation table. Our
  tool surface is already generated, so the server is small, and it removes rmcp's
  API churn. The fallback is rmcp pinned to an exact version behind our own module.
  That's a decision for the user.
- **Build** the Prometheus text exposition encoder inside the Prometheus out connector.
  The format is small, and status is already channels (C7).

## 9. InfluxDB, Ignition, Grafana outbound

**InfluxDB.**
- The v1, v2, and v3 HTTP write APIs all accept line protocol (earlier research). v3
  defaults to `accept_partial=true` and `no_sync`, so the connector must set both
  explicitly.
- `influxdb3-client` 0.3.0 was created 2026-06-08 (4k downloads). `influxdb2` 0.5.2 is
  stale (2024-07). `influxdb` 0.8.0 is a v1-era client.
- **Verdict: build** a line-protocol encoder (small) on one shared HTTP client. hyper
  1.11.1 (MIT) is the candidate. Whether it fits our runtime seam is fork 1's call.

**Ignition.**
- Ignition's OPC UA client is built in and connects to third-party OPC UA servers even
  without the OPC UA module (Inductive Automation 8.3 docs).
- 8.3 also has Event Streams, with a Kafka source (consumer group or partition) and a
  Kafka handler (Inductive Automation 8.3 docs and Inductive University).
- MQTT Engine (Sparkplug) is a paid Cirrus Link module (earlier research).
- The Module SDK is Java and needs signing (earlier research).
- **Verdict: no Ignition library and no Java module.** Ignition reaches Foundation
  through our OPC UA server (area 1), Sparkplug B (area 3), and Kafka (area 4).

**Grafana.**
- Grafana Live accepts line protocol at `POST /api/live/push/:streamId` and stores
  nothing (Grafana docs and earlier research).
- Signing a plugin for distribution under the "Commercial" level needs a Commercial
  Plugin Subscription. "Community" signing is free only for plugins with no commercial
  affiliation (grafana.com/legal/plugins and the plugin signature docs).
- **Verdict: no Grafana plugin.** Grafana reads Foundation data from the stores it
  already supports (InfluxDB and Prometheus out connectors), and live panels can use
  Live push with the line-protocol encoder from InfluxDB.

## Cross-cutting findings

1. **One crypto provider.** aws-lc-rs everywhere: rustls, iroh, OPC UA, signing, and
   sealed secrets. It's the only required C dependency.
2. **Five protocol stacks are ours and sans-I/O**: Modbus, MQTT, Sparkplug, Kafka, and
   the MCP server. They run in the simulator, and the T1 layer 7 protocol simulators
   reuse each codec's server side.
3. **Three stay outside the simulator by nature**: async-opcua (behind the connector
   seam), the DAQmx and LJM vendor libraries, and the HTTP client (behind a seam).
4. **License obligations to track**:
   - MPL-2.0 (async-opcua): tell recipients where the source is, and publish our
     changes to its files, including the forked crypto crate.
   - EPL-2.0 (the Tahu proto, if used).
   - Attribution for implementing the Sparkplug spec.
   - Vendor runtimes are never redistributed.
5. **Size and memory are unmeasured.** Add a `bench/size` job to fork 1's harness that
   builds the node for aarch64 Linux and records binary size and idle RSS for each
   feature (opcua server namespace, kafka, fips), measured against P1's limits (Pi 4,
   idle under 50 MB).

## Decisions for the user

1. **OPC UA** (superseded: see "Revised decision 1" below): adopt async-opcua and fork
   only its crypto crate onto aws-lc-rs, instead of building our own stack or wrapping
   open62541 through FFI.
2. **Sparkplug schema source**: generate from the Tahu proto (EPL-2.0, legal check
   owed) or write the schema from the spec text (recommended: it avoids the question,
   and the TCK proves compatibility either way). Passing the TCK is required by the
   patent license in either case.
3. **Kafka**: build a pure-Rust client on generated codecs (recommended: no C, runs in
   simulation, and KIP-848 shrinks consumer groups) or wrap librdkafka behind a build
   flag (faster to ship, but brings C and cross-compile cost).
4. **NI header**: get a legal check on vendoring `NIDAQmx.h` from `ni/grpc-device`, or
   declare only the functions we call by hand from NI's public C reference (recommended
   if the check is slow).
5. **FIPS**: ship a separate FIPS build of the binary (CMake and Go in CI, certificate
   #4816 scope) now, or later when a customer asks.
6. **MCP**: build a thin MCP server generated from the operation table (recommended), or
   adopt rmcp pinned to an exact version behind our own module.

Areas 2, 5 (apart from the header question), 6, 7 (apart from FIPS), 8 (apart from
MCP), and 9 need no decision: the verdict follows from the rules already locked.

## Sources

- crates.io API and GitHub API metadata, pulled 2026-10-04, for every crate and repo
  named above.
- async-opcua: repository source (`async-opcua-crypto/Cargo.toml`,
  `async-opcua-client/src/transport/connect.rs`, TODO.md);
  https://github.com/freeopcua/async-opcua
- RUSTSEC-2023-0071: https://osv.dev/vulnerability/RUSTSEC-2023-0071,
  https://rustsec.org/advisories/RUSTSEC-2023-0071
- open62541 ECC: https://open62541.org/doc/1.5/ecc_security.html,
  https://open62541.org/doc/1.5/security/backends.html
- open62541 EventLoop: https://open62541.org/doc/1.5/plugin_eventloop.html
- open62541 certification:
  https://www.iosb.fraunhofer.de/en/press/press-releases/2019/open62541-opc-ua-stack-en.html,
  https://open62541.org/certification
- open62541 Rust crate: https://docs.rs/open62541
- rmodbus: https://docs.rs/crate/rmodbus/0.6.1; tokio-modbus: https://docs.rs/tokio-modbus/
- serial2: https://github.com/de-vri-es/serial2-rs
- Sparkplug FAQ and patent license: https://sparkplug.eclipse.org/about/faq/,
  https://sparkplug.eclipse.org/specification/
- EPL-2.0 text: https://gitlab.eclipse.org/eclipse/technology/dash/eclipse-project-code/-/blob/main/LICENSES/EPL-2.0.txt
- rumqttc health: https://depscope.dev/pkg/cargo/rumqttc,
  https://github.com/bytebeamio/rumqtt/releases
- rskafka README: https://github.com/influxdata/rskafka
- KIP-848: https://kafka.apache.org/40/operations/consumer-rebalance-protocol/,
  https://www.confluent.io/blog/kip-848-consumer-rebalance-protocol/
- kafka-protocol: https://github.com/tychedelia/kafka-protocol-rs
- ni-daqmx-sys `build.rs`: https://github.com/WiresmithTech/ni-daqmx-sys
- NI header: https://github.com/ni/grpc-device (`imports/include/NIDAQmx.h`)
- async_labjack: https://github.com/nschrading/async_labjack
- pco paper: https://arxiv.org/html/2502.06112v2
- FastLanes: https://github.com/spiraldb/fastlanes; ALP: https://ir.cwi.nl/pub/33334,
  https://github.com/spiraldb/alp
- rustls FIPS: https://docs.rs/crate/rustls/0.23.43/source/src/manual/fips.rs
- aws-lc-rs requirements: https://aws.github.io/aws-lc-rs/requirements/windows.html,
  https://aws.github.io/aws-lc-rs/platform_support.html
- ring maintenance: https://rustsec.org/advisories/RUSTSEC-2025-0007,
  https://www.infoq.com/news/2026/09/rustls-one-decade/
- MPL-2.0 FAQ: https://www.mozilla.org/en-US/MPL/2.0/FAQ/
- Grafana Live: https://grafana.com/docs/grafana/latest/setup-grafana/set-up-grafana-live/
- Grafana plugin signing: https://grafana.com/legal/plugins,
  https://grafana.com/docs/grafana/latest/administration/plugin-management/plugin-sign/
- Ignition OPC UA client:
  https://docs.inductiveautomation.com/docs/8.3/ignition-modules/opc-ua/opc-ua-connections
- Ignition Event Streams:
  https://docs.inductiveautomation.com/docs/8.3/ignition-modules/event-streams/types-of-sources

## Compiled-in C libraries

Follow-up from the user: re-evaluate every area with a fourth option: our own Rust
bindings to a C library compiled statically into the binary (cc or zig for every
target). This section changes the recommendation for decision 1 only.

Method:
- Sizes were measured on 2026-10-04 on macOS arm64 (Apple clang 21, `-Os`, dead-strip,
  stripped). An empty C program is 16.8 KB on the same setup. aarch64 Linux sizes will
  differ, so treat these as indicative.
- CVE counts come from an NVD keyword search, with each hit read by hand. Thread and
  clock claims come from a grep of each library's source at its current head.

| Area | Best C candidate | Maturity | License (static link) | Our network and clock? Own threads? | Memory-safety record | Size (measured) | Beats earlier verdict? |
|---|---|---|---|---|---|---|---|
| 1 OPC UA | open62541 1.5 | **OPC Foundation certified**: Standard 2017 UA Server Profile | MPL-2.0: OK | **Yes**: EventLoop plugin. No threads. | **Poor**: 25 CVEs, 22 in 2026, one at CVSS 9.8 | Server 1.40 MB, client 0.27 MB (no crypto) | **Yes, narrowly** (decision 1) |
| 2 Modbus | nanoMODBUS (MIT); libmodbus (LGPL-2.1) | Small or mid-size community | MIT OK; LGPL needs relinking | nanoMODBUS: callbacks, but blocking. libmodbus: own sockets. | nanoMODBUS: 5 CVEs in 2026, up to 9.8. libmodbus: 8 since 2022. | nanoMODBUS 16 KB | No |
| 3 MQTT | coreMQTT 5.0.2 (MIT) | AWS FreeRTOS LTS, MISRA checks, CBMC proofs | MIT OK | **Yes**: send, recv, and time are callbacks. No threads, no malloc. But connect blocks. | 1 CVE (v5 property parser, 2026) | 42 KB | No, narrowly |
| 4 Kafka | librdkafka 2.16.0 (BSD-2) | Base of Confluent's Go, Python, .NET clients | BSD-2 OK | **No**: 2 threads + 1 per broker, reads system clock | 0 NVD CVEs; OSS-Fuzz | Producer 1.47 MB (no TLS, SASL, codecs) | No (it is the "wrap" option) |
| 5 DAQmx, LJM | None: vendor code is closed | n/a | n/a | n/a | n/a | n/a | No change |
| 6 Codecs | zstd, lz4; cwida FastLanes and ALP (C++) | zstd and lz4 very high | BSD-style, MIT OK | Pure functions: OK | zstd library itself: few CVEs | not measured | No for ALP and FastLanes; zstd OK if fork 2 wants it |
| 7 TLS, crypto | aws-lc (already compiled in); OpenSSL 3, mbedTLS | Very high | ISC/Apache OK | rustls keeps the protocol in Rust | aws-lc-sys: 10 OSV advisories, mostly PKCS7/X.509/CRL | not measured | Already this option |
| 8 CLI, config | None worth it | | | | | | No change |
| 9 HTTP out | libcurl | Very high | curl (MIT-style) OK | Partly: `multi_socket` lets our loop drive it, but it reads its own clock | 215 CVEs on curl's own feed, 45 in 2026 | not measured | No |

### 1. OPC UA: open62541 compiled in

- **Certification (verified):** the open62541 1.4.0-rc2 sample server is certified for
  the Standard 2017 UA Server Profile. Certificate 2404CE010C, issued 2024-04-12, valid
  to 2027-12-31. It covers six security policies, three user token types, history, and
  alarms and conditions (open62541.org/certification, OPC Foundation 2024-05-23
  announcement). async-opcua has no certification.
- **ECC:** 1.5 has the ECC policies today (NIST P-256 and P-384, brainpool, Curve25519,
  Curve448), through OpenSSL 3 or mbedTLS 3.
- **T1 fit (verified in source):**
  - `struct UA_EventLoop` has `run(timeout)`, `addTimer`, `dateTime_now`, and
    `dateTime_nowMonotonic`. Network I/O goes through ConnectionManager plugins. So we
    can write our own EventLoop and ConnectionManager over the injected clock and
    network, and run the server in the simulator. async-opcua's server cannot do this.
  - 89 clock reads go through the EventLoop. 18 call the global `UA_DateTime_now()`
    directly. Only 2 of those are in the base server path; the rest are in GDS,
    alarms, PubSub, the history backend, config, logging, and the POSIX arch layer. We
    can supply our own `UA_DateTime_now` through a custom arch layer.
  - The amalgamated source creates no threads.
- **C2 fit:** it is single-threaded and runs one instance per shard. Build with
  `UA_MULTITHREADING=0` to drop its mutexes.
- **Crypto gap:** the OpenSSL plugin uses OpenSSL 3-only APIs (`OSSL_PARAM`,
  `OSSL_KDF_PARAM_*`). Whether AWS-LC can serve it is **UNVERIFIED** and likely needs
  patches. There are two choices:
  - write our own SecurityPolicy plugin on aws-lc (one crypto library), or
  - compile in mbedTLS 3 (Apache-2.0) as a second crypto library.
- **Memory safety:**
  - 25 NVD CVEs, 22 of them in 2026. CVE-2026-67870 (CVSS 9.8) is in AddReferences.
    CVE-2026-65423 (8.8) is an out-of-bounds write in the core Variant decoder.
    CVE-2026-63035 and CVE-2026-67863 are use-after-free bugs in subscriptions.
  - Many sit in features we can compile out: LDS and mDNS, GDS, the history memory
    backend, PubSub, and node management. The decoder and subscription bugs remain.
  - Six are in the client, which a hostile server can reach.
  - It is in OSS-Fuzz. async-opcua has 0 OSV advisories, but far fewer people have
    looked at it.
- **Cross-compile:** the amalgamation is one `.c` and one `.h` (174k lines). It builds
  with the `cc` crate. POSIX and Win32 arch layers are in the tree.
- **Size:** the server with reduced namespace 0 and no crypto is 1.40 MB; the client
  is 0.27 MB.
- **Verdict: compiled-in open62541 beats async-opcua.** It gives a certified profile,
  ECC today, and a server that runs in the simulator. These outweigh async-opcua's
  memory safety, but only if we accept four duties:
  - a trimmed feature set,
  - our own fuzzing in CI on top of OSS-Fuzz,
  - a fast patch-and-release duty for its CVE stream, and
  - the crypto plugin choice above.

  Isolation idea for the boundary fork: run C protocol stacks in a child process of the
  same binary, so that memory corruption cannot reach the store. This costs IPC and
  makes simulation harder. **Not evaluated.**

### 2. Modbus

- libmodbus 3.1 (LGPL-2.1):
  - Static linking triggers LGPL-2.1 section 6: we must ship, or offer, our object
    files so users can relink.
  - It owns its sockets and termios, and its calls block.
  - It has 8 NVD CVEs since 2022, including heap and stack overflows.
- nanoMODBUS (MIT, 916 stars, 2.5k lines, 16 KB):
  - Read and write are callbacks, which is good. But they take byte timeouts and must
    block until data arrives, so they don't fit a non-blocking shard loop.
  - Five CVEs in 2026, including out-of-bounds writes at CVSS 9.8 (CVE-2026-71254,
    CVE-2026-71256).
- **No change.** Our Rust codec is the same size of work, and Rust removes the
  out-of-bounds class that both libraries keep hitting.

### 3. MQTT and Sparkplug

- **coreMQTT 5.0.2** (MIT, AWS) is the best-fitting C library in this whole audit:
  - Transport `send`/`recv` and `getTime` are injected. It has no threads and no
    malloc.
  - It passes MISRA checks and has CBMC memory-safety proofs (AWS and NXP docs).
  - MQTT 5 support is new: coreMQTT 5.0.0, shipped in the FreeRTOS 202604 LTS. A bounds
    bug in the new property parser was already found (CVE-2026-8686, fixed in 5.0.1).
  - `MQTT_Connect` polls `recv` in a loop until CONNACK or timeout
    (`core_mqtt.c:3458`), so connect blocks the shard.
  - It is 42 KB.
- Eclipse Paho C (EPL-2.0 or BSD-3) starts send and receive threads in `MQTTAsync.c`
  and a run thread in `MQTTClient.c`. Rejected.
- libmosquitto (EPL-2.0 or BSD-3) offers an external-loop API, but it opens its own
  sockets (`net_mosq.c`) and reads its own clock. Its client also had CVE-2024-10525
  (CVSS 9.8, a crafted SUBACK). Rejected.
- Tahu C (EPL-2.0) is only a Sparkplug payload codec on nanopb.
- **No change, narrowly.** A Rust state machine without I/O is about the same work as
  the FFI and callback glue around coreMQTT, and it avoids the blocking connect. Use
  coreMQTT as a differential-test reference.

### 4. Kafka

- librdkafka 2.16.0 (BSD-2):
  - Confluent's Go, Python, and .NET clients wrap it (their READMEs). It has 0 NVD CVEs
    and is in OSS-Fuzz.
  - **Threads:** it starts a main thread, an internal broker thread, and one thread per
    broker (`thrd_create` in `rdkafka.c`, `rdkafka_broker.c`,
    `rdkafka_background.c`; Karafka's thread docs).
  - **Clock:** `rd_clock()` reads the system clock directly (`rdtime.h:85`).
  - So it cannot be injected, and it conflicts with T1 and C2.
  - A minimal producer, with no TLS, SASL, or compression, is 1.47 MB.
  - Bundled dependencies (OpenSSL, zlib, zstd, lz4, Cyrus SASL, libcurl) add build
    work per target.
- **No change.** Static librdkafka was already the "wrap" option in decision 3.

### 5. NI DAQmx and LabJack LJM

- **No change.** DAQmx and LJM are closed vendor binaries and cannot be compiled in.
- LabJack's Exodriver (`liblabjackusb`) is a raw USB transport for T4 and T7 devices
  on Linux and macOS only. It depends on libusb (LGPL-2.1, so it carries the relinking
  duty).
- If we want USB T-series devices without LJM, nusb 0.2.7 (pure Rust, Apache-2.0 or
  MIT) plus our Modbus is the cleaner path. That the T-series uses Modbus framing over
  USB is **UNVERIFIED**.

### 6. Codecs

- cwida FastLanes (C++, MIT, 714 stars) and cwida ALP (C++, MIT, last push
  2025-10-16) are the reference implementations. They would add a C++ standard library
  to the static link.
- zstd (BSD-3 or GPL-2.0) and lz4 (BSD-2 library) are mature. An NVD search finds few
  CVEs against the zstd library itself; most keyword hits are other products.
- All of these are pure functions, so T1 and C2 are fine.
- **No change** for ALP and FastLanes: the format is ours and the algorithms are small.
  zstd compiled in is an acceptable C dependency if fork 2 wants a general entropy
  stage.

### 7. Crypto and TLS

- **Already this option:** aws-lc is C, compiled in by `aws-lc-sys`.
- Swapping rustls for a C TLS stack (OpenSSL 3 or mbedTLS) would move the
  network-facing handshake parser back into C. wolfSSL is GPL or commercial.
- aws-lc-sys has 10 OSV advisories, mostly in PKCS7, X.509, and CRL code. rustls
  verifies certificates in Rust (webpki), so we do not expose that code to the network.
- **No change.**

### 8. CLI, MCP, config, metrics

- **No change.** No C library improves on the Rust picks.

### 9. HTTP for InfluxDB, Grafana, and Ignition

- libcurl (curl license, MIT-style):
  - `curl_multi_socket_action` lets our loop drive sockets and timers, and
    `CURLOPT_OPENSOCKETFUNCTION` lets us supply sockets. But it reads its own clock.
  - curl's own feed lists 215 CVEs, 45 published in 2026.
- **No change.**

### Cross-cutting

- **Toolchain:** cargo-zigbuild supports Linux, macOS, and Windows GNU targets, not
  MSVC (cargo-zigbuild docs, zig forum). Windows MSVC builds of C code need clang-cl or
  MSVC through the `cc` crate. Every compiled-in C library adds this per-target matrix.
- **Licenses:**
  - Keep LGPL code (libmodbus, libusb) out of the binary; section 6 relinking duties
    fit badly with one BSL binary.
  - MPL-2.0 is fine.
  - Take Paho and Mosquitto under BSD-3, not EPL-2.0.
- **CVE load:** C network parsers took many CVEs in 2026 (open62541 22, curl 45,
  nanoMODBUS 5). Each compiled-in C parser adds an SBOM entry, a watch, and a
  rebuild-and-release duty.

### Revised decision 1 (OPC UA)

Pick one:
- **open62541 compiled in (recommended).** Certified, ECC today, and the server runs in
  the simulator through our own EventLoop. In return we take a C CVE stream, a trimmed
  build, fuzzing, and a crypto plugin choice: our own on aws-lc, or mbedTLS.
- **async-opcua plus a crypto fork.** Memory-safe Rust, but uncertified, no ECC yet, and
  a server that stays outside the simulator.

Decisions 2-6 are unchanged.

### Sources for this section

- open62541 certification: https://open62541.org/certification,
  https://opcfoundation.org/?p=19517
- open62541 EventLoop: https://open62541.org/doc/1.5/plugin_eventloop.html, plus a
  source grep of the v1.5 amalgamation
- open62541 crypto backends: https://open62541.org/doc/1.5/security/backends.html
- open62541 CVEs: NVD keyword search, and
  https://security-tracker.debian.org/tracker/source-package/open62541
- OSS-Fuzz projects: https://github.com/google/oss-fuzz/tree/master/projects/open62541,
  https://github.com/google/oss-fuzz/tree/master/projects/librdkafka
- coreMQTT: https://docs.aws.amazon.com/freertos/latest/userguide/coremqtt.html,
  https://mcuxpresso.nxp.com/mcuxsdk/26.03.00/html/rtos/freertos/coremqtt/README.html,
  https://aws.amazon.com/about-aws/whats-new/2026/04/freertos-lts/, CVE-2026-8686
- nanoMODBUS: https://github.com/debevv/nanoMODBUS, plus NVD CVE-2026-29972, -54410,
  -71254, -71255, -71256
- libmodbus CVEs: NVD, https://vulnerability.circl.lu/vuln/CVE-2022-0367
- LGPL-2.1 section 6: https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html
- Paho threads: `paho.mqtt.c/src/MQTTAsync.c:688,693`; license: EPL-2.0 or EDL-1.0
  (BSD-3)
- libmosquitto external loop: https://mosquitto.org/api/files/mosquitto-h.html;
  CVE-2024-10525
- librdkafka threads: https://karafka.io/docs/Librdkafka-Threads-and-Pipe-Patterns/,
  plus a source grep
- curl CVE feed: https://curl.se/docs/vuln.json
- cargo-zigbuild targets: https://pypi.org/project/cargo-zigbuild/,
  https://ziggit.dev/t/how-to-correctly-compile-static-libraries-on-windows/4232
- aws-lc-sys advisories: https://osv.dev (crates.io `aws-lc-sys`)
