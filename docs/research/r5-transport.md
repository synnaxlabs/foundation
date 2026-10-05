# Foundation research 5: the transport (iroh or our own)

Fork 5 of 8, 2026-10-04. Verification marks: **[V2]** two or more independent sources,
**[V1]** one primary source (code, owner docs, or the paper itself), **[UNVERIFIED]**
reasoning or a single secondary source only.

## Summary of recommendations

| # | Question | Recommendation |
|---|---|---|
| 1 | Requirements | The list holds, with eight corrections (below). The biggest: addresses come from the mesh, not DNS; a TCP carrier is mandatory; latest mode needs "drop stale" semantics that plain streams lack; one QUIC connection is near the P1 throughput limit. |
| 2 | Candidates | Only two fit the T1 injection rule and the feature set: noq-proto (n0's sans-I/O QUIC with multipath and NAT traversal) and quinn-proto (sans-I/O, no multipath). iroh's `Endpoint` does not fit T1 or C2. |
| 3 | Performance | One userspace QUIC connection moves 2.4 to 8.2 Gbit/s on a 10 Gbit/s link and is bound by one core; GSO/GRO and large packets are the main levers. The 250 us p99 target has no published QUIC measurement and must be measured early. |
| 4 | Relays | Relays run inside designated Foundation nodes (a policy, like voters), speak TLS over TCP 443 so they pass UDP-blocking firewalls and CONNECT proxies, and admit only keys in the spec. No n0 infrastructure. |
| 5 | One-way links | Supportable, but not by QUIC. A separate small one-way carrier (UDP + Noise K pattern + RaptorQ FEC) plus a codec that can run without feedback. Commands, Raft, and clock exchange cannot cross a diode. |
| 6 | Path | Build our own transport on noq-proto, driven by our shard event loops, with three carriers for QUIC packets (direct UDP, direct TCP, relay over 443) and addresses from the mesh. Do not adopt iroh's `Endpoint`. |

**The single decision for the user:** build Foundation's transport on noq-proto (a
sans-I/O QUIC core with multipath and NAT traversal) with our own I/O driver, relay,
and addressing, instead of adopting iroh's `Endpoint` behind a Transport trait.

---

## 1. Requirements: confirmed, with corrections

**Recommendation:** keep the list, apply the corrections below.

Confirmed from the locked design: dial by public key (S8); NAT traversal; relays;
outbound-only connections from remote sites; end-to-end encryption; priorities between
channels on a shared link (B6 deferred priority to the wire section); unreliable
delivery for latest mode (B4); multipath for Starlink plus cellular; injectable time,
randomness, and I/O (T1); P1 targets; Linux, macOS, Windows on x86-64 and ARM.

Corrections and additions:

1. **Addresses come from the mesh, not DNS.** Node public keys are in the spec (S8) and
   current addresses are gossip hints (S9). iroh's DNS and pkarr address publishing is
   not needed; bootstrap addresses live in config. This removes a whole subsystem.
2. **Key rotation (S8).** The transport identity is the node's current public key;
   rotation changes it, and peers learn the new key from the spec. No transport-level
   identity beyond the key.
3. **A TCP carrier is mandatory, not optional.** Industrial networks often block UDP
   (research ledger section 3: IEC 62443 conduits, Purdue DMZ). iroh's own enterprise
   guide says its relay needs outbound TCP 443 with WebSocket upgrades allowed and that
   web filters which block upgrades stop iroh entirely
   (https://docs.iroh.computer/configuring-networks) [V1]. Some OT LANs block UDP even
   between segments, so a direct TCP path (not only a relay) is required.
4. **Latest mode needs "drop stale," which plain QUIC streams do not give.** A lost
   packet on a stream is retransmitted even when a newer frame has replaced it. QUIC
   datagrams avoid that but are capped at one packet (about 1.2 KB, research ledger
   "Riskiest"). The proven pattern is Media over QUIC: one stream per group, reset by
   the sender or cancelled by the receiver when a newer group supersedes it, plus a
   max-latency drop rule (https://datatracker.ietf.org/doc/html/draft-lcurley-moq-lite-05)
   [V1]. For Foundation: a latest-mode frame larger than one packet goes on its own
   short stream that is reset when superseded; small frames may use datagrams.
5. **One connection per peer pair is near the P1 limit.** 100M samples/s at under 4
   bytes per sample is up to 3.2 Gbit/s to one remote complete reader. Measured single
   QUIC connections reach 2.4 to 8.2 Gbit/s on 10 Gbit/s links, bound by one core
   (section 3). The transport must allow several connections per peer (for example one
   per shard, C2), and GSO/GRO is required, not a tuning option.
6. **Cipher choice on Raspberry Pi 4.** The Pi 4's BCM2711 lacks the ARMv8 crypto
   extensions, so AES-GCM runs in software; ChaCha20-Poly1305 must be preferred there
   [UNVERIFIED: one search summary plus common knowledge; the S4/C2 benchmark should
   measure it on a Pi 4].
7. **Version negotiation.** C9d requires one integer version per wire format and
   reading the previous version. ALPN carries the Foundation protocol version per
   connection.
8. **One-way links are a separate requirement** (question 5); QUIC cannot serve them.

Dropped from the list: "every node can relay." A relay needs a reachable address, and
many OT nodes have none (research ledger "Riskiest"). Relays are designated nodes.

---

## 2. Candidates

**Recommendation:** noq-proto is the only candidate that meets T1 and the feature set;
quinn-proto is the fallback. Everything else is rejected.

| Candidate | Fit | Maturity and maintenance | Performance evidence | T1 injection | Platforms |
|---|---|---|---|---|---|
| **iroh 1.3 `Endpoint`** | Dial by key, NAT traversal (~90% direct), relays, multipath, datagrams. Brings DNS/pkarr discovery, portmapper, its own path selection. | 1.0 on 2026-06-15 with a stable wire protocol and API; 1.3.0 on 2026-09-28; 65 pre-releases over four years (https://iroh.computer/blog/v1, https://docs.rs/crate/iroh/latest) [V2]. 200,000+ concurrent production connections (noq announcement) [V1]. | "Gigabit-class" with worker tuning; no published 10 Gbit/s or latency numbers found [UNVERIFIED]. | **No.** Owns a Tokio runtime and reads the clock; issue #4459 (open) adds host time sources only for WASM, behind an unstable flag (https://github.com/n0-computer/iroh/issues/4459) [V1]. Custom transports are `unstable-custom-transports`, may change without notice [V1]. | Linux, macOS, Windows, Android, iOS, WASM; armv7 not in iroh CI (ledger) |
| **noq / noq-proto** (n0's Quinn fork) | Sans-I/O QUIC with multipath (draft), NAT traversal (QNT draft), address discovery (QAD, replaces STUN), RFC 9221 datagrams, stream priority (`SendStream::set_priority(i32)`) (https://github.com/n0-computer/noq, docs.rs noq-proto) [V2]. | Forked 2024 because path, relay, and NAT logic could not live outside QUIC (https://www.iroh.computer/blog/why-we-forked-quinn) [V1]; noq 1.0 2026-06-15, 1.3 with iroh 1.3; Apache-2.0/MIT. | Inherits Quinn's I/O work (quinn-udp GSO/GRO). | **Yes.** "None of the functions actually perform system-level I/O"; every timed call takes `now: Instant`; `EndpointConfig::rng_seed(Option<[u8; 32]>)` for deterministic randomness (docs.rs noq-proto `Connection`, `EndpointConfig`) [V2]. | Linux, Windows, macOS, Android, iOS, WASM (README) [V1] |
| **quinn / quinn-proto** | Sans-I/O core, datagrams, stream priority; no multipath, no NAT traversal. | Since 2018, 30+ releases, active (https://github.com/quinn-rs/quinn) [V1]. Firefox ships quinn-udp (Inden, MIR 2025) [V1]. | quinn-udp alone: 370-426 MiB/s without GSO, 7.8-9.6 GiB/s with GSO on localhost, UDP I/O only, no QUIC crypto (quinn PR #1915) [V1]. Firefox: close to 4 Gbit/s loopback with GRO [V1]. An older single-connection test: 206 Mbit/s at 81% CPU vs msquic 46% (search summary) [UNVERIFIED]. | **Yes**, same design as noq-proto (docs.rs quinn-proto) [V2]. | Linux, macOS, Windows |
| **s2n-quic** (AWS) | Full QUIC, GSO, CUBIC, pacing; no multipath or NAT traversal found. | Active (https://github.com/aws/s2n-quic) [V1]. | No published numbers found. | Partly: AWS's `bach` simulator exists (docs.rs bach) but the README does not describe a sans-I/O core [UNVERIFIED]. | Linux (kernel 5.0+), macOS, Windows (rustls on MSVC) [V1] |
| **quiche** (Cloudflare) | Sans-I/O C/Rust API; needs BoringSSL (C dependency). | Mature. | 3.1 to 5.8 Gbit/s on 10 Gbit/s depending on offload and MTU; sharp drop with RTT (0.15 Gbit/s at high RTT) (Koenig et al. 2025) [V1]. | Yes (sans-I/O) | Wide |
| **msquic** (Microsoft) | Fastest; XDP kernel bypass; C library with its own thread pool. | Mature. | XDP "more than doubles RPS" and cuts latency (Microsoft) [V1]; Tailscale used it for 10 Gbit/s tests [V1]. | **No.** Own threads and clock. | Windows, Linux, macOS |
| **Raw UDP + Noise (WireGuard-style, boringtun)** | Encryption only. We would rebuild reliability, ordering, flow and congestion control, and multiplexing, which is rebuilding QUIC. Tailscale gets reliability by running kernel TCP inside the tunnel, which needs a TUN device or a userspace TCP stack. | boringtun is "undergoing a restructuring"; users told not to rely on master (https://github.com/cloudflare/boringtun) [V1]. | wireguard-go reached 7.2 to 13 Gbit/s with TUN GSO/GRO (Tailscale, 2023) [V2]. | Yes, but irrelevant | Wide |
| **TCP + TLS** | Lowest CPU per byte (kernel, kTLS on Linux). One ordered byte stream per connection: head-of-line blocking across channels, no datagrams, no migration or multipath; NAT traversal for TCP is weaker. | Mature. | Saturates 10 Gbit/s where QUIC does not (Koenig et al. 2025) [V1]. | Socket I/O injectable; kernel TCP itself is not simulated | Wide |
| **libp2p (rust)** | Wraps quinn for QUIC; relay v2; DCUtR hole punching at 70% ± 7.1% success (https://arxiv.org/html/2510.27500v1) [V1]. Peer and protocol model far larger than we need. | Active but focused on reducing maintenance burden (roadmap) [V1]. | No relevant numbers. | No | Wide |
| **Tailscale's design** | Not a library; the prior art for everything iroh does: WireGuard, coordination server, hole punching, DERP relays over HTTPS 443, and since 2025 "peer relays" on customer nodes over UDP with DERP fallback (https://tailscale.com/blog/peer-relays-beta, https://tailscale.com/kb/1082/derp-servers) [V2]. | Production at scale. | 10 Gbit/s club with GSO/GRO (above). | n/a | n/a |

Rejected, with the concrete downside:
- **iroh `Endpoint`:** the production transport would never run under simulation. T1's
  layer 2 would test our sim implementation of the Transport trait, not iroh, so bugs
  in path switching, relay fallback, and multipath would only appear in the
  non-deterministic patchbay tests or in production. It also owns its runtime, which
  conflicts with C2's shard-per-core proposal, and it brings DNS discovery, portmapper,
  and relay-server dependencies we do not use.
- **msquic:** fastest, but a C library with its own threads and clock; fails T1 and the
  library rule.
- **Noise or WireGuard as the base:** rebuilds QUIC by hand.
- **TCP + TLS as the base:** head-of-line blocking across channels and no multipath for
  Starlink plus cellular. Kept only as a carrier for QUIC packets (question 6).
- **libp2p:** a large framework around quinn with weaker hole punching than iroh's
  reported ~90%.
- **s2n-quic, quiche:** no multipath or NAT traversal; quiche adds a C crypto
  dependency.

---

## 3. Measured userspace QUIC performance

**Recommendation:** plan for one core per QUIC connection, require GSO/GRO, allow
several connections per peer, and measure the 250 us p99 target in the C2 benchmark
before locking anything that depends on it.

Evidence:
- **Single-connection throughput.** On 10 Gbit/s links, TCP and plain UDP fill the link
  while QUIC implementations reach 2.4 to 8.22 Gbit/s; "throughput limitations stem
  primarily from single-core performance constraints" (Koenig, Rust, Zitterbart,
  Scheuermann, KIT and TU Darmstadt, 2025, quoting their earlier study) [V1, peer
  reviewed]. The same paper: packet processing, not the link, is the bottleneck;
  GSO/GRO plus a 9000-byte MTU let ngtcp2 reach 9.97 Gbit/s and lsquic 9.19 Gbit/s;
  without them, 3.5 to 5 Gbit/s.
- **Gap to TCP.** Over fast Internet paths, UDP+QUIC+HTTP/3 lost up to 45.2% of the data
  rate of TCP+TLS+HTTP/2; root cause "high receiver-side processing overhead,"
  "excessive data packets and QUIC's user-space ACKs" (Zhang et al., WWW 2024,
  https://arxiv.org/abs/2310.09423) [V2 with Koenig et al.].
- **GSO/GRO.** quinn-udp: about 20x for UDP sends on localhost (PR #1915) [V1]. Firefox
  with quinn-udp GRO: the 75th percentile read syscall returns 2+ packets, the 95th
  returns 10+; "close to 4 Gbit/s" loopback; Linux GSO max 10 datagrams, GRO 64 KB
  buffer; **Windows USO and URO disabled** because some drivers report no segment size,
  lose packets, or crash (Inden, MIR 2025 slides) [V1]. Tailscale saw 3.3x to 4.3x from
  GSO/GRO in wireguard-go and found deep CPU sleep states cut forwarding to 1.4 Gbit/s
  until limited (https://tailscale.com/blog/quic-udp-throughput) [V1].
- **CPU per Gbit/s.** No clean published figure for a Rust QUIC stack [UNVERIFIED]. The
  "one core per 2.4 to 8 Gbit/s" bound above is the best available proxy.
- **Latency.** No published request-response p99 for userspace QUIC on a LAN was found
  [UNVERIFIED]. Zenoh reports 16 us average between two machines over TCP (ledger). The
  risks for QUIC tails are ACK timers, scheduler wakeups, and CPU sleep states, not the
  wire. **Action:** add a ping-pong p50/p99 measurement over noq-proto on the C2
  benchmark machine, with and without CPU sleep states limited.
- **What limits one connection:** per-packet crypto and ACK processing on one core;
  1200 to 1500-byte packets on the Internet; per-datagram syscalls without GSO/GRO;
  receive-side ACK generation (Zhang et al.).

Consequences for Foundation:
- Large frames (B6 smart batching) keep packets full, which is the main lever.
- One connection per shard per peer (C2) spreads crypto and ACK work across cores.
- Jumbo frames (MTU 9000) on site LANs are worth supporting through path MTU discovery.
- Windows will be slower until its offloads are reliable; P1 should be measured on Linux
  first and reported per OS.

---

## 4. Relay design

**Recommendation:** relays are designated Foundation nodes chosen by a policy, speak
TLS over TCP 443 (and pass HTTP CONNECT proxies), admit only keys in the spec, and are
the last resort after direct UDP and direct TCP.

- **Embedded, not hosted.** Tailscale moved the same way: "peer relays" let any
  customer node relay over UDP, ahead of Tailscale's managed DERP servers, because they
  are "less throughput-constrained" and work behind strict firewalls
  (https://tailscale.com/blog/peer-relays-beta) [V2 with the KB]. iroh relays are
  self-hostable and its guide shows replacing n0's relays and DNS
  (`presets::Empty` plus a custom `RelayMap`) [V1]. Foundation never depends on n0's
  public relays or DNS.
- **Which nodes relay.** A relay needs a reachable address, so relaying is a role chosen
  by policy, like voters (K5) and time sources (C6), not "every node." Typical relays:
  cloud nodes, and a DMZ node at each site.
- **Access control.** iroh-relay has an `AccessControl` trait ("controls which endpoints
  may use the relay") and per-client rate limits (`Limits`, `ClientRateLimit`)
  (docs.rs iroh_relay::server) [V1]. Foundation's rule: a relay forwards only between
  keys present in the spec it holds; anything else is refused at connect.
- **UDP-blocking firewalls.** DERP and the iroh relay both carry encrypted packets over
  HTTPS on TCP 443 [V2]. iroh supports HTTP CONNECT proxies (`proxy_from_env`,
  `proxy_url`) and a system CA option for TLS-inspecting proxies [V1]. Foundation needs
  the same three: TLS on 443, CONNECT proxy support, and an option to trust the system
  CA store. Using plain TLS framing instead of a WebSocket upgrade avoids the failure
  iroh documents with filters that block upgrades [UNVERIFIED: some filters also block
  non-HTTP TLS; support both framings if field reports require it].
- **Direct TCP before relay.** Inside a site LAN that blocks UDP, a direct TLS-over-TCP
  path avoids the relay hop. With noq's "relay as a path" model, a direct TCP connection
  is simply another path carrying QUIC packets [UNVERIFIED that noq-proto accepts
  arbitrary packet carriers without changes; iroh's relay path shows the pattern works].
- **Cost of embedding iroh-relay's server crate:** its `server` feature forces
  `tls-ring` and pulls ACME, clap, and toml (research ledger section 9). A relay that
  forwards encrypted packets by destination key is small; iroh's whole `iroh-relay/src`
  is about 449 KB of Rust including server, client, ACME, and metrics (GitHub tree, this
  research) [V1].

---

## 5. One-way links (data diodes, receive-only satellite)

**Recommendation:** support them with a separate one-way carrier, and design the codec
and spec distribution so they can run without feedback; do not bend the main transport
around them.

- **QUIC cannot cross a diode.** Its handshake, ACKs, flow control, and congestion
  control all need a return path [V2: protocol design; diode literature describes UDP
  with FEC instead of TCP].
- **Industry practice.** Diodes carry UDP with forward error correction because there is
  no return channel; standard industrial protocols do not pass, so vendors proxy or
  replicate on each side (Cogent DataHub white paper on access through a diode; USPTO
  11362762 on error-correcting one-way transfer) [V2]. Research ledger section 3: "data
  diodes use one-way UDP with FEC."
- **What the carrier needs:**
  - Encryption without a handshake: the Noise framework defines one-way patterns N, K,
    and X; K fits because both static keys are in the spec
    (https://en.wikipedia.org/wiki/Noise_Protocol_Framework and the Noise spec) [V2].
  - FEC: RaptorQ (RFC 6330). The `raptorq` crate encodes at 18.8 to 26.9 Gbit/s on a
    Ryzen 9 9950X3D (crate README, 2.0.1) [V1]; overhead is a per-link setting.
  - Explicit sequence numbers on every frame and periodic keyframes, because the
    receiver can never ask the stateful codec (A8) to resynchronize.
- **What it requires of complete mode:** no acknowledgment can return, so no reader on
  the far side can hold data at the home (S10 hold is meaningless), and the sender
  cannot know what arrived. The receiving side detects missing sequence ranges and
  records them as explicit gaps, the same gap marker as B1 and B5. Optional repeated
  sending (a carousel) raises completeness at a bandwidth cost.
- **What cannot cross:** commands and acks (the purpose of a diode), Raft (the far side
  needs its own voters for its branch, K5), clock exchange (each side uses its own time
  source, C6). Spec branches for the channels that cross are pushed one way, checked by
  hash (S9).
- **Satellite links:** Starlink is two-way with handovers at seconds 12, 27, 42, and 57
  of each minute, causing RTT spikes and loss that congestion control misreads; QUIC
  multipath across Starlink and cellular is the remedy (research summaries, Starlink
  measurement papers) [V2: UVic LEONet 2024, Glasgow eprints]. GEO links: about 600 ms
  RTT and performance-enhancing proxies cannot split encrypted QUIC (ledger) [V1].
  True receive-only satellite broadcast is the diode case.

---

## 6. Recommendation: which path

**Recommendation:** build Foundation's own transport on **noq-proto**.

Shape:
- **Core:** noq-proto connections, owned by the shard that owns the indexes they carry
  (C2), driven by our event loop. Time comes from the injected clock, randomness from
  `rng_seed`, packets from the injected network. The same state machine runs in
  production and in the T1 simulator.
- **Carriers for QUIC packets:** direct UDP (noq-udp or our own driver with GSO/GRO),
  direct TCP with TLS, and relay over TLS on 443 through designated relay nodes.
- **Addressing:** keys and bootstrap addresses from the spec; current addresses as
  gossip hints (S9); QAD for reflexive addresses; noq's QNT for hole punching.
- **Delivery mapping:** complete mode on ordered streams with priorities; latest mode on
  a stream per frame that is reset when superseded (Media over QUIC pattern), with
  datagrams for frames under one packet; commands at the highest priority.
- **One-way carrier:** separate, small, used only where a diode exists.

Risks of this path:
- **We own the glue iroh already wrote.** Path selection, relay fallback, candidate
  addresses, and connection management sit in iroh's `iroh/src/socket`, about 446 KB of
  Rust (GitHub tree) [V1]. Ours can be smaller because addressing comes from the mesh,
  but hole-punch success must be measured before we claim iroh-like rates.
- **Dependency on n0's fork.** noq-proto is maintained by n0 for iroh. If they diverge
  from our needs, we fork a sans-I/O core (noq-proto/src is about 2.1 MB including tests)
  or fall back to quinn-proto and lose multipath and NAT traversal.
- **Draft standards.** Multipath and QNT are IETF drafts; wire changes may come with noq
  releases. Mitigation: pin versions; Foundation's own ALPN version (C9d) gates upgrades.
- **Performance is unproven for our latency target** (question 3).

Risks of adopting iroh `Endpoint` behind our own Transport trait:
- Production transport never runs under simulation (T1 layer 2 tests a stand-in).
- Its runtime and task model conflict with C2's shard ownership; tuning is limited to
  its knobs.
- Larger dependency set (DNS discovery, portmapper, relay server deps) for features we
  replace with the mesh; unstable APIs for the parts we need (custom transports, time).
- Upside: fastest to a working mesh, proven NAT traversal at 200,000+ connections.

Risks of building on quinn-proto instead:
- We implement multipath and NAT traversal ourselves, the work n0 forked Quinn to do.
- Upside: the most conservative upstream.

A practical hedge, if the user wants speed first: prototype the first mesh on iroh
`Endpoint` to learn the field behavior, while the transport crate grows on noq-proto
behind the same Transport trait; switch before the first stable release (RFC 0058 owes
no compatibility to candidates).

---

## Out of scope notes for other forks

- C2 (fork 1): one QUIC connection is bound by one core; the shard model should own
  connections per peer. CPU sleep states changed Tailscale's forwarding throughput by
  7x; the benchmark should record the C-state setting.
- S3/S4 (fork 2): the one-way carrier needs explicit sequence numbers and keyframes in
  the codec.
- C6 (fork 6): clock exchange over relayed or asymmetric paths gives poor offsets; time
  sources should prefer direct paths and must not run across a diode.

## Sources

- iroh 1.0 announcement: https://iroh.computer/blog/v1
- iroh crate 1.3.0: https://docs.rs/crate/iroh/latest
- iroh enterprise networking guide: https://docs.iroh.computer/configuring-networks
- iroh issue #4459: https://github.com/n0-computer/iroh/issues/4459
- iroh-relay server docs: https://docs.rs/iroh-relay/latest/iroh_relay/server/index.html
- noq repository: https://github.com/n0-computer/noq
- noq-proto docs: https://docs.rs/noq-proto/latest/noq_proto/struct.Connection.html,
  https://docs.rs/noq-proto/latest/noq_proto/struct.EndpointConfig.html,
  https://docs.rs/noq-proto/latest/noq_proto/struct.SendStream.html
- Why we forked Quinn: https://www.iroh.computer/blog/why-we-forked-quinn
- noq announcement: https://iroh.computer/blog/noq-announcement
- quinn repository: https://github.com/quinn-rs/quinn; quinn-proto docs:
  https://docs.rs/quinn-proto/latest/quinn_proto/struct.Connection.html
- quinn PR #1915: https://github.com/quinn-rs/quinn/pull/1915
- Inden, "Fast UDP makes QUIC quicker," MIR 2025:
  https://www.ce.cit.tum.de/fileadmin/w00cgn/cm/mir-2025/talks/2025-inden-talk-fast-udp.pdf
- Koenig et al. 2025, QUIC throughput landscape:
  https://doc.tm.kit.edu/2025-Examining-the-Heterogeneous-Throughput-Performance-Landscape-of-QUIC-Implementations-Koenig-et-al.pdf
- Zhang et al., "QUIC is not Quick Enough over Fast Internet," WWW 2024:
  https://arxiv.org/abs/2310.09423
- Tailscale UDP/QUIC throughput: https://tailscale.com/blog/quic-udp-throughput
- Tailscale peer relays: https://tailscale.com/blog/peer-relays-beta,
  https://tailscale.com/kb/1591/peer-relays; DERP: https://tailscale.com/kb/1082/derp-servers
- s2n-quic: https://github.com/aws/s2n-quic; bach: https://docs.rs/bach/latest/bach
- msquic: https://github.com/microsoft/msquic
- boringtun: https://github.com/cloudflare/boringtun
- libp2p hole punching campaign: https://arxiv.org/html/2510.27500v1,
  https://probelab.io/blog/can-libp2p-punch-through-nats/
- moq-lite draft: https://datatracker.ietf.org/doc/html/draft-lcurley-moq-lite-05
- Noise one-way patterns: https://en.wikipedia.org/wiki/Noise_Protocol_Framework
- raptorq crate: https://docs.rs/crate/raptorq/latest
- Data diode practice: https://cogentdatahub.com/access-data-through-diode/,
  https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11362762
- Starlink handovers: https://onlineacademiccommunity.uvic.ca/starlink/wp-content/uploads/sites/8876/2024/09/leonet24-victor.pdf,
  https://eprints.gla.ac.uk/390916/
- Pi crypto: https://bench.cr.yp.to/web-impl/aarch64-pi5-crypto_aead-aes256gcmv1.html
