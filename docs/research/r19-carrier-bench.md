# R19: QUIC against TLS over TCP on Linux

Date: 2026-10-05. Issue #10, as R14 asks. The benchmark is `bench/carrier/` at commit
`a96d83d` (PR #32). The data is in `r19-carrier-bench/`: `bulk.md`, `latency.md`,
`load.md`, and `run.log`. "(inference)" marks my own reasoning. Every other number is
measured in this run. Each figure is the median of 3 reps, with the range of the reps
in brackets.

## Summary

| Class | Carrier | Why |
|---|---|---|
| Command, Latest | QUIC | No carrier wins under the rule, which then picks QUIC. Under bulk on the same connection, the paced p99 is 0.76 to 0.82 ms on QUIC and 2.21 ms on TLS, but that test favors QUIC (see Method). |
| Complete, CatchUp | TLS over TCP | 9.16 Gbit/s against 5.56, at 1.61 ns/B of CPU against 2.46, both by more than the spread. |

- TRANSPORT SURFACE runs every class of a session on one carrier until a measurement
  shows that `Latest` p99 holds while `CatchUp` runs on the other carrier. With one
  carrier, I recommend QUIC (inference). Under bulk on the same connection, its
  `Latest` p99 is 2.7 to 2.9 times lower than on TLS, though QUIC gave its paced
  stream priority and its bulk ran slower (6.5 to 6.8 Gbit/s against 9.15). Its
  bulk rate with GSO is 3.2 to 6.5 Gbit/s across all runs. P1's 100M samples/s at 4
  bytes is 3.2 Gbit/s if all of it crosses one connection, so the slow runs have no
  margin.
- `link-tls` comes near the split that R14 asks for on QUIC streams: paced p99 with
  two TLS bulk flows on the link is 698 us (593 to 1000), against 698 us (697 to 761)
  with no load. On QUIC datagrams it is 797 us (777 to 876) against 704 us (690 to
  777): the gap is inside the spread, but each load rep is at or above the highest
  no-load rep. The split is not proved: the flows were separate processes, not one
  session, and the run did not measure it in the awake profile.
- No carrier meets P1 latency in the default power profile (inference): the paced
  round trip p99 is 698 to 893 us with no load. P1 counts one hop (p99 under 250 us),
  and this run measured round trips only. With the CPUs kept out of idle states, the
  round trip p99 is 102 to 113 us. Most of the default round trip is the CPU wake
  from deep idle states (inference). TLS is 190 us above QUIC there, but 11 us below
  in the awake profile.
- The paced timer fires 0.6 to 2.1 ms after its due time at p99, in each profile.
  Tokio's 1 ms timer causes it (inference: the delay stays 1.0 to 1.5 ms in the
  awake profile). A `Latest` send must not wait on a Tokio timer (inference).
- On this CPU, QUIC costs 1.17 ns/B on the sending thread and TLS 0.34 ns/B. So one
  core caps one QUIC connection near 6.8 Gbit/s (inference).

## Method

**Carriers.** QUIC is noq 1.3.0 (noq-proto 1.3.0) with Cubic, GSO, a fixed UDP payload
of the link MTU less 28 B (no MTU discovery), an 8 MiB stream window, a 16 MiB
connection window, and 1 MiB datagram buffers. TLS is tokio-rustls 0.26.6 (rustls
0.23.45) with `TCP_NODELAY` and a 16 KiB TLS send buffer. Both use TLS 1.3 with
AES-128-GCM on aws-lc-rs 1.18.1. Each side runs on a current-thread Tokio 1.53.2
runtime, pinned to one CPU.

**Tests.** A frame is a little-endian `u32` length and the payload.

- `bulk`: one stream of 64 KiB frames for 20 s. `unsegmented` turns off QUIC GSO.
- `ping`: one echo frame in flight for 20 s. Sizes 64 B to 64 KiB on streams; 64,
  256, and 1024 B on QUIC datagrams.
- `paced`: 256 B echo frames at 1000/s for 60 s, under four loads:
  - `none`: no other traffic.
  - `shared`: a bulk flow on the same connection and thread. On QUIC, the paced
    stream has priority 1 and the bulk stream priority 0. On TLS, one TCP stream
    carries both, each bulk frame one 16 KiB TLS record, and a due paced frame waits
    for the record in progress. So the two carriers did not carry the same load:
    QUIC bulk ran at 6.5 to 6.8 Gbit/s, TLS bulk at 9.15.
  - `link-quic` and `link-tls`: two bulk flows of that carrier, in other processes on
    CPUs 4 and 5, to other ports. They share the link, the NIC queues, and the IRQ
    CPUs with the measured run.

The first second of each run is a warmup. A round trip starts when the timer fires,
so the timer delay does not enter it. Each path then sends at once, except TLS under
`shared`: there the round trip includes the wait for the bulk record in progress.
"delay" is how late the timer fired after the due time; the Tokio interval uses
`MissedTickBehavior::Burst`. A datagram with no echo after 100 ms is lost. Within a
profile, the order of the tests changes each rep; the order of the profiles is
fixed.

**CPU.** "thread ns/B" is the CPU time of the run thread per payload byte. "CPUs ns/B"
is the busy time (user, nice, system, IRQ, softirq) of the run CPU and the IRQ CPUs per
byte. "client + server CPUs" adds both hosts.

**Counters**, summed over both hosts and over the whole host, load flows included:
nstat `TcpRetransSegs`, `UdpRcvbufErrors`, `UdpSndbufErrors`, and `UdpInErrors`; the
sum of the ENA `*_allowance_exceeded` counters; and qdisc drops.

**Profiles** (GRO, GSO, MTU, `tcp_notsent_lowat`, idle states). TSO is off in each
(see Machines).

| Profile | GRO | GSO | MTU | lowat | CPU idle states |
|---|---|---|---|---|---|
| default | on | on | 1500 | 16384 | on |
| none | off | off | 1500 | 16384 | on |
| gro-only | on | off | 1500 | 16384 | on |
| gso-only | off | on | 1500 | 16384 | on |
| jumbo | on | on | 9001 | 16384 | on |
| lowat-off | on | on | 1500 | 4294967295 | on |
| awake | on | on | 1500 | 16384 | off (all but POLL) |

**Host setup during the run.** Measured client and servers on CPU 2, NIC interrupts
on CPUs 8 to 15, RPS off, irqbalance stopped. `rmem_max` and `wmem_max` 16 MiB,
`rmem_default` 4 MiB; `wmem_default` stays 212992 B. noq sets no socket buffer size,
so each QUIC socket has a 4 MiB receive buffer and a 208 KiB send buffer. The script
puts each setting back at the end.

**Command.** From the repo root on a third machine:
`bench/carrier/run.sh ubuntu@<server> ubuntu@<client> 3`. It ran from 06:02 to 07:36
UTC with exit 0: 3 reps of 51 runs (39 latency, 12 bulk), and 6 link-load pairs
in each rep.

## Machines

- 2 AWS c7i.8xlarge in us-east-1b, in one placement group: Intel Xeon Platinum 8488C,
  32 vCPUs (2 threads per core), 1 NUMA node.
- Ubuntu 24.04.5, kernel 7.0.0-1013-aws, `CONFIG_HZ=1000`,
  `CONFIG_IRQ_TIME_ACCOUNTING` not set. intel_idle with POLL, C1, C1E, and C6; no
  frequency governor.
- ENA, 8 combined queues, RX ring 1024 (max 8192), TX ring 1024 (max 1024), adaptive
  RX coalescing on. No TSO or UDP segmentation offload in the NIC: the kernel
  segments in software.
- MTU 1500, qdisc `mq` with `fq_codel`, Cubic. `tcp_rmem` 4096 131072 33554432;
  `tcp_wmem` 4096 16384 4194304.
- rustc 1.98.1, release build.

## Results

### Command and latest: paced round trip, default profile

p99 in us. p50 and p99.9 are in `latency.md`.

| Load | QUIC datagram | QUIC stream | TLS |
|---|---|---|---|
| none | 704 (690 to 777) | 698 (697 to 761) | 893 (818 to 918) |
| shared | 757 (488 to 1308) | 822 (391 to 1174) | 2215 (2156 to 2562) |
| link-quic | 750 (646 to 907) | 703 (668 to 880) | 904 (888 to 956) |
| link-tls | 797 (777 to 876) | 698 (593 to 1000) | 938 (739 to 3029) |

- Under `shared`, the TLS p50 is 1568 us, against 252 to 257 us on QUIC. On TLS the
  paced frame waits behind bulk bytes on one ordered stream, and a lost segment holds
  both (inference). TLS had a median of 1794 retransmitted segments per run.
- Under `shared`, the QUIC p50 (252 to 257 us) is lower than with no load (486 to
  490 us). A busy CPU does not enter a deep idle state (inference, from the awake
  profile below).
- The `link-tls` loads ran at 11.93 Gbit/s in each run, near the 12.5 Gbit/s limit
  of the instance. The `link-quic` loads ran at 6.4 to 11.8 Gbit/s. The ENA allowance
  counters rose by 70 thousand to 80 million in those runs, and no paced datagram was
  lost. AWS counts packets that it queued or dropped (AWS docs).
- Under `shared`, QUIC lost 1 paced datagram in rep 1 and 1 in rep 3.
- The paced delay p99 is 617 to 2135 us across loads and carriers.
- QUIC streams and QUIC datagrams do not differ by more than the spread.

### Ping, default profile

p50 and p99 in us. The awake profile is for comparison.

| Frames | 64 B | 1 KiB | 16 KiB | 64 KiB | 64 B, awake |
|---|---|---|---|---|---|
| QUIC datagram | 570 / 827 | 582 / 838 | | | 62 / 91 |
| QUIC stream | 575 / 832 | 572 / 831 | 631 / 878 | 653 / 951 | 62 / 80 |
| TLS | 573 / 791 | 576 / 800 | 715 / 1180 | 795 / 1284 | 52 / 66 |

With idle states on, each ping wakes a CPU from a deep idle state, so the round trip
p50 is about 570 us for each carrier at 64 B and 1 KiB (inference). With idle states
off, it is 52 to 62 us. At 4 KiB the TLS p50 is lower, 443 us (435 to 461); I did not
find the cause. At 16 KiB and above, TLS has a higher p99 than QUIC.

### Awake profile, paced, no load

| Carrier | p50 us | p99 us | delay p99 us |
|---|---|---|---|
| QUIC datagram | 68 | 113 (106 to 114) | 1043 |
| QUIC stream | 69 | 113 (111 to 114) | 1083 |
| TLS | 58 | 102 (101 to 111) | 1493 |

### Complete and catch-up: bulk

| Profile | Carrier | Gbit/s | client thread ns/B | server thread ns/B | client + server CPUs ns/B |
|---|---|---|---|---|---|
| default | QUIC | 5.56 (4.91 to 5.96) | 1.17 | 0.86 | 2.46 (2.38 to 2.48) |
| default | TLS | 9.16 (all reps) | 0.34 | 0.47 | 1.61 (1.59 to 1.77) |
| default, unsegmented | QUIC | 3.72 (3.71 to 3.76) | 2.01 | 1.03 | 3.45 (3.33 to 3.79) |
| none | QUIC | 5.05 (3.63 to 6.51) | 1.19 | 1.15 | 3.02 (2.91 to 3.04) |
| none | TLS | 9.16 | 0.36 | 0.56 | 2.10 (1.58 to 2.21) |
| gro-only | QUIC | 4.94 (3.73 to 6.23) | 1.21 | 0.88 | 2.42 (2.41 to 2.62) |
| gso-only | QUIC | 5.28 (3.48 to 5.40) | 1.19 | 1.13 | 3.01 (2.85 to 3.05) |
| jumbo | QUIC | 3.51 (3.51 to 3.57) | 1.25 | 0.95 | 2.36 (2.35 to 2.38) |
| jumbo | TLS | 9.51 | 0.31 | 0.34 | 1.26 (1.24 to 1.26) |
| lowat-off | TLS | 9.16 | 0.26 | 0.47 | 1.42 (1.18 to 1.63) |

- TLS ran at 9.16 Gbit/s in each rep and each profile at MTU 1500, and 9.51 at MTU
  9001, with its client thread at 29 to 44% of a core. A cap of the link or of one
  flow sets it, not the CPU (inference). AWS limits one flow to 10 Gbit/s in a
  cluster placement group and to 5 Gbit/s outside one (AWS docs).
- The QUIC client thread used 73 to 87% of a core in the default profile, and the
  socket send buffer was full 20,033 to 36,155 times per run (`UdpSndbufErrors`).
- The QUIC rate varies more than the default profile shows. Across all runs with
  GSO, bulk and link loads, it is 3.2 to 6.5 Gbit/s. In the 9 of 33 runs below 4
  Gbit/s, the client thread used 54 to 63% of a core, and each slow bulk run had
  51,714 to 55,827 send buffer errors.
- GSO in noq lowers the client thread cost from 2.01 to 1.17 ns/B and raises the rate
  from 3.72 to 5.56 Gbit/s.
- QUIC at MTU 9001 is slower than at 1500, with about 55,000 send buffer errors per
  run. I did not find the cause.
- With `tcp_notsent_lowat` off, the TLS client thread cost falls from 0.34 to 0.26
  ns/B. A 16 KiB low mark wakes the sender more often (inference).
- No bulk run had an ENA allowance count or a qdisc drop.

## The rule, applied

The rule was posted on #10 before the run.

**Latest and command.** The carrier with the lowest paced p99 wins when its gap to the
next carrier is larger than the spread of both, under at least two of the three
loads. Otherwise QUIC wins.

- `shared`: QUIC datagram 757 against QUIC stream 822, gap 65, spreads 821 and 783:
  no winner.
- `link-quic`: QUIC stream 703 against QUIC datagram 750: no winner. QUIC stream
  against TLS: gap 201, spreads 212 and 68: no winner.
- `link-tls`: QUIC stream 698 against QUIC datagram 797: no winner. Against TLS: gap
  240, spreads 407 and 2289: no winner.

No carrier won under any load, so QUIC wins by the default. Outside the rule: under
`shared`, TLS is above both QUIC variants by more than the spreads (against QUIC
stream: gap 1393, spreads 783 and 406), in a test that favors QUIC (see Method).

**Complete and catch-up.** A carrier wins when it is better on both Gbit/s and CPU
ns/B by more than the spread. TLS: 9.16 against 5.56 Gbit/s (gap 3.60, spreads 0 and
1.05), and 1.61 against 2.46 ns/B (gap 0.85, spreads 0.19 and 0.10). TLS over TCP
wins.

## What this means

1. **One carrier per session (now).** QUIC. It keeps `Latest` near its no-load p99
   when bulk shares the connection, with its paced stream at a higher priority. Its
   bulk rate is at or above P1's byte rate, with no margin in the slow runs
   (inference). The cost is 1.5 times the CPU per byte and 60% of the TLS rate for
   `Complete` and `CatchUp`.
2. **A split (later).** QUIC for `Command` and `Latest`, TLS over TCP for `Complete`
   and `CatchUp`. `link-tls` suggests that `Latest` p99 holds. Measure it again
   with Foundation's own transport, two carriers in one session, in the default and
   awake profiles, before the split becomes a default.
3. **P1 latency** needs CPUs that stay out of deep idle states, by host settings or
   a busy core. A node with default power settings gives a round trip near 0.7 to
   0.9 ms on this hardware. The deployment guide must say this (inference).
4. **Timers.** The send path of `Latest` and `Command` runs on data arrival. A pacing
   timer needs a finer resolution than Tokio's 1 ms (inference).
5. **QUIC rate.** In the slow QUIC runs, the client thread used 54 to 63% of a core
   while the send buffer was full most often. So the socket send buffer or the
   kernel limits QUIC there, not the thread (inference). A send buffer larger than
   208 KiB and NICs with UDP segmentation offload are the next levers (inference, r5
   section 3).
6. **Not answered here:** whether decode reads QUIC chunks in place (TRANSPORT
   SURFACE). This benchmark copies each frame out of the stream.

## Limits

- One cloud instance type and one CPU (Sapphire Rapids). ENA has no TSO or USO, and
  AWS limits each flow and the whole instance. Bare metal NICs, ARM, and the
  Raspberry Pi 4 are not measured.
- 3 reps. The spreads of the paced p99 under load are wide (up to 2.3 ms on TLS), so
  small gaps are not significant.
- The kernel has no IRQ time accounting, so IRQ and softirq time comes from tick
  samples at 1000 Hz. The "CPUs ns/B" figures are noisy (the TLS `lowat-off` range
  is 1.18 to 1.63), and they undercount: in 3 QUIC runs, the client CPUs figure is
  below the client thread figure, which it includes.
- The counters cover the whole host, load flows included.
- One current-thread Tokio runtime per side; no busy polling.
- noq with its default settings apart from the windows and buffers above. noq's
  socket send buffer size and pacing are not tuned. quinn-proto was not run.
- The link loads ran in separate processes on separate connections, not as classes of
  one session.
- The awake profile ran with no load.
- Round trips only. One-way latency is not measured.
