# Research fork 1: C2 thread model

Date: 2026-10-04. Scope: the home's hot path (sequence numbers, control gate, disk
buffer, reader fan-out per index), runtimes, iroh and noq runtime needs, blocking
vendor libraries, sans-I/O, and a local benchmark.

## Recommended C2 shape (one sentence)

Work runs where its data lives: one shard per core owns a set of indexes with no locks,
each connector task runs on the shard that owns the indexes it writes, frames from the
network take one batched handoff to the owning shard, every shard runs a Tokio
`LocalRuntime`, blocking vendor calls run on one OS thread per device, and protocol
cores are sans-I/O state machines.

This refines the earlier C2 proposal. The earlier version handed every frame from a
shared Tokio pool to a shard. The benchmark below shows that the handoff, not the shard,
is the expensive part, so local device data should never take it. On Linux with pinned
threads, a handoff to a parked shard costs 4 to 11 us, with no millisecond tail (Linux
rerun).

## Q1. Shards vs work-stealing vs hybrid

**Recommendation:** shard per core owning indexes, with "work steering" (the task that
produces an index's frames runs on that index's shard), plus index rebalancing between
shards for hot spots.

Evidence:

- Redpanda pins one thread per core, each core owns a disjoint set of partitions, no
  shared mutable state, cross-core work by explicit message passing (Seastar SMP).
  Sources: https://www.redpanda.com/blog/what-makes-redpanda-fast,
  https://docs.redpanda.com/24.3/get-started/architecture,
  https://www.redpanda.com/blog/tpc-buffers.
- Apache Iggy (Rust streaming server, Apache project) moved from Tokio work-stealing
  (v0.5.0) to thread-per-core on compio (v0.7.0), 2026-02-27. 32 producers x 32
  streams: throughput flat at ~1,000 MB/s, P99 4.52 ms -> 1.82 ms, P999 5.43 -> 2.38
  ms, P9999 27.52 -> 11.83 ms. With fsync, 32 partitions: 931 -> 1,102 MB/s, P95 33.98
  -> 18.49 ms. Gains grow with partition count. Problems they hit: RefCell borrows
  held across `.await` panic, manual hot-spot handling, background-task consistency.
  Source: https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/.
- PulseBeam (Rust WebRTC SFU) moved from Tokio work-stealing to a Tokio `LocalRuntime`
  per thread, each thread owning a shard: P99.99 end-to-end 70 ms -> 10 ms, capacity
  +25%. Downsides they report: harder load balancing, one hot connection can saturate a
  core. Source: https://pulsebeam.dev/blog/moving-to-thread-per-core.
- Enberg, Rao, Tarkoma (ANCS 2019): a partitioned key-value store with inter-thread
  messaging cut tail latency by up to 71% vs Memcached. Source:
  https://penberg.org/papers/tpc-ancs19.pdf (via search result mirror).
- ByteDance monoio benchmark (Xeon Gold 5118, 10 GbE, 100-byte payloads): thread-per-
  core about 2x Tokio's peak at 4 cores and close to 3x at 16 cores; Tokio has lower
  latency at 1 core with few connections. Source:
  https://github.com/bytedance/monoio/blob/master/docs/en/benchmark.md.
- TigerBeetle runs its state machine on one thread with static allocation and io_uring
  because contended state gains nothing from more threads. Source:
  https://docs.tigerbeetle.com/concepts/performance/.
- Local benchmark (Q6): co-located shards scale linearly and have the lowest tails;
  shared state halves scaling at 8 threads when per-frame work is light; a handoff
  queue adds the worst tails.

Counter-evidence (honest trade): without.boats argues the Enberg result uses uniform
load, and work-stealing wins under real imbalance (hot keys, uneven requests).
Source: https://without.boats/blog/thread-per-core/. Iggy and PulseBeam both report
manual load balancing as the main cost. For Foundation the unit of imbalance is an
index, so the mitigation is moving an index between shards (needs a short ownership
transfer protocol: drain, hand over state, redirect producers).

Rejected:

- **Tokio work-stealing with shared state behind locks.** Measured 2x worse scaling
  at 8 threads with light work (4.9 vs 9.3 G samples/s), 2-3x worse p99 at 80M
  samples/s, and 5-10x worse tails when two writers share an index (p99.9 115 us vs
  10 us at 8 passes). Every shared map needs a lock or atomics that bounce cache lines
  between cores. Violates the user's "minimize locking at all costs".
- **Shards fed only by handoff from a shared Tokio pool** (my original C2). Same lock
  freedom inside shards, but every local frame crosses cores, and on macOS the
  handoff tail reached p99 0.18-1 ms and p99.9 up to 8.6 ms at 40-80M samples/s.
- **One thread for everything (TigerBeetle).** Caps a node at one core; P1's 100M
  samples/s and 100k channels per node need several cores.

## Q2. Runtimes for thread-per-core on Linux, macOS, Windows (x86-64, ARM)

**Recommendation:** one Tokio `LocalRuntime` per shard thread. It is stable since Tokio
1.51.0 (2026-04-03), runs on mio (epoll on Linux, kqueue on macOS, IOCP on Windows),
and is the runtime iroh, async-opcua, and tokio-modbus already need. Disk I/O goes
behind the injected disk interface (T1) with an OS-specific backend, decided in S4.

| Runtime | Platforms and driver | State (crates.io, 2026-10-04) | Verdict |
|---|---|---|---|
| Tokio `LocalRuntime` | Linux epoll, macOS kqueue, Windows IOCP (mio) | tokio 1.53.2, 2026-10-03; LocalRuntime stabilized in 1.51.0 (CHANGELOG line 232, added unstable in 1.41.0) | Use |
| compio | Linux io_uring + polling, Windows IOCP, macOS kqueue/polling | 0.19.2, 2026-09-29, active; pre-1.0, no MSRV promise; driver separate from executor; has compio-quic and compio-tls | Candidate disk backend or reference; not Tokio, so iroh and connector crates need a compat layer |
| monoio | Linux io_uring, mio fallback elsewhere | 0.2.4, 2024-08-20; Iggy found it behind on io_uring features and lightly maintained | Reject |
| glommio | Linux only | 0.9.0, 2024-03-25; Iggy calls it "pretty much unmaintained" | Reject (no macOS, Windows) |
| tokio-uring | Linux only | 0.5.0, 2024-05-27 | Reject |

Sources: crates.io API queried 2026-10-04; local tokio-1.53.1 source
(`src/runtime/mod.rs:619-620` exports `LocalRuntime` without `cfg_unstable`;
`src/runtime/local_runtime/runtime.rs:14-31` docs); https://github.com/compio-rs/compio;
Iggy blog above.

Notes:
- Tokio's file I/O uses the shared blocking pool (default 512 threads). Iggy left Tokio
  for this reason. Foundation's disk buffer should not use `tokio::fs`.
- Thread pinning: `core_affinity::set_for_current` returns false on macOS (the
  benchmark reports `pinned=false`). macOS only offers affinity hints. Pinning works on
  Linux and Windows.

## Q3. Can iroh (noq) and quinn run on one runtime per core?

**Recommendation:** yes, but run one iroh endpoint per node on one network shard, and
hand frames to owning shards; do not create one endpoint per core.

Evidence:
- noq's `Runtime` trait requires `Send + Sync` and spawns `Send` futures
  (`noq/src/runtime/mod.rs:18-31`); its Tokio implementation calls `tokio::spawn` and
  reads time from `tokio::time::Instant::now()` (`noq/src/runtime/tokio.rs:28-40`). Any
  Tokio runtime works, including current-thread and `LocalRuntime` (a `LocalRuntime`
  handle accepts `Send` tasks: tokio `local_runtime/runtime.rs:100-102`).
- iroh's own tests run on a current-thread runtime with paused time:
  `#[tokio::test(flavor = "current_thread", start_paused = true)]`
  (`iroh/src/endpoint.rs:3118`), and a doc example builds a current-thread runtime
  (`iroh/src/endpoint.rs:1449`). iroh's bench maps 1 worker to `new_current_thread`
  (`iroh/bench/src/lib.rs:158-168`). No `block_in_place` in iroh or noq code (GitHub
  code search; only an old CHANGELOG mention).
- One endpoint is one identity and one UDP socket. Several endpoints sharing one key
  would split connections across sockets and confuse address lookup (inference, not
  tested).

Note for fork 5 (transport): `start_paused` with noq reading `tokio::time::Instant`
suggests Tokio's paused clock already reaches noq timers, which bears on the iroh
simulated-time gap (issue #4459). Unverified.

Risk: one network shard limits a node's network ingest to one core's QUIC crypto and
packet work. A cloud node that receives many sites could hit it. Measure in the
transport research.

## Q4. Blocking vendor libraries (NI DAQmx, LabJack LJM)

**Recommendation:** each device handle gets its own OS thread (a device actor) that
owns the handle and makes every blocking call; it hands one frame per read (N samples)
to the owning shard through the same bounded queue. Never call vendor code on a shard
or on Tokio's shared blocking pool.

Evidence:
- Synnax runs each acquisition pipeline on its own `std::thread`
  (`driver/pipeline/base.h:23`, `:62`), and the research ledger records the LJM handle
  race that made "one device actor owns each handle" a lesson.
- LabJack: LJM is thread-safe and lets threads share a handle, but `LJM_Close` from any
  thread closes the handle for all threads, so threads must coordinate closing. Source:
  https://support.labjack.com/docs/is-ljm-thread-safe. One owner thread removes the
  coordination.
- DAQmx read calls block up to their timeout (NI forum threads, e.g.
  https://forums.ni.com/t5/Multifunction-DAQ/DAQmxReadCounterF64-timeouts-blocking-program-runtime/m-p/2798536).
  DAQmx thread-safety guarantees: unverified; one owner thread per task is safe either
  way.
- Iggy's move away from Tokio's blocking pool (Q2).

Cost: one cross-core handoff per device read. Reads carry hundreds or thousands of
samples, so the handoff cost per sample is small.

## Q5. Sans-I/O cores as a principle

**Recommendation:** yes for protocol cores (consensus, the wire session and codec
state, the control gate, spec sync, clock offset estimation, replication): pure state
machines that take input and the current time as arguments and return outputs, driven
by thin async drivers. Not for connectors or glue code.

For:
- quinn-proto, rustls, raft-rs (tick-driven core), str0m (WebRTC), sctp-proto, and
  Firezone's connlib all use it. Firezone passes `Instant` into every function instead
  of calling `Instant::now`. Sources: https://www.firezone.dev/blog/sans-io,
  https://docs.rs/crate/str0m/latest, https://docs.rs/crate/sctp-proto/latest.
- It satisfies T1 by construction: a state machine with no I/O and no clock runs
  unchanged in simulation and in production.
- It is runtime-independent, so it fits shards with `LocalRuntime`, and avoids
  RefCell-across-await panics (Iggy) because the state machine is not async.

Against:
- Hand-written state machines give up async/await ergonomics and add boilerplate
  (users.rust-lang.org discussions, asansio crate README:
  https://docs.rs/crate/asansio/0.5.0/source/README.md).
- Who owns input buffering (driver or state machine) must be decided per protocol.

Scope rule that keeps the cost low: sans-I/O where correctness is subtle and DST
matters most; plain async elsewhere, with clock and network still injected.

## Q6. Benchmark

Project: `scratchpad/tpc-bench/` (standalone Cargo project, outside the repo).

Machine: Apple M3 Max, 16 cores (12 performance + 4 efficiency), 48 GB, macOS 27.0.1
(26A434). Toolchain: rustc 1.98.1 (Homebrew). Crates: tokio 1.53.2, rtrb 0.4.0,
parking_lot 0.12.5, hdrhistogram 7.6.0, core_affinity 0.8.3. Release build, thin LTO,
1 codegen unit.

Commands:

```
cd scratchpad/tpc-bench && cargo build --release
SECS=2 WORK=1 SHARDS=4 ./target/release/tpc-bench   # run-work1.txt
SECS=2 WORK=1 SHARDS=8 ./target/release/tpc-bench   # run-work1-s8-x3.txt (3 runs)
SECS=2 WORK=8 SHARDS=8 ./target/release/tpc-bench   # run-work8-s8.txt
```

Designs, all doing identical per-frame work (gate check, `WORK` passes over 1,000
f64, timestamp read, sequence update, bounded ring push, fan-out to 2 reader slots), 64
indexes, payloads prebuilt per producer and shared by `Arc` (no allocation on the hot
path):
- **A handoff**: P Tokio producer tasks on a P-worker runtime hand frames to S shard
  threads through per-(producer, shard) `rtrb` SPSC rings (1,024 slots); shards poll,
  spin, then yield.
- **B shared**: P Tokio tasks on a (P+S)-worker work-stealing runtime process inline
  against `RwLock<HashMap<index, Mutex<IndexState>>>` (parking_lot).
- **C co-located**: S threads, each a Tokio `LocalRuntime`; producers run on the shard
  that owns their indexes and process inline with `RefCell` state.

Headline results (WORK=1, ~0.8 us of work per frame; 3 runs, within ~5%):

| Unthrottled throughput (G samples/s) | P=1 | P=4 | P=8 |
|---|---|---|---|
| A handoff (S=8) | 8.7 | 8.7-8.9 | 7.8-8.7 |
| B shared (W=P+8) | 1.24-1.26 | 4.2 | 4.9 |
| C co-located (S=P) | 1.29-1.32 | 4.9 | 9.1-9.3 |

A's unthrottled number reflects 8 shards working in parallel behind 1 producer; its
latency there is queueing and not meaningful.

| Paced 10,000 frames/s per producer, P=8 (80M samples/s) | p50 | p99 | p99.9 |
|---|---|---|---|
| A handoff (S=8) | 2.1-2.4 us | 183-263 us | 0.82-4.6 ms |
| A handoff (S=4) | 1.9 us | 33 us | 6.2 ms |
| B shared | 1.8-1.9 us | 3.2-3.5 us | 4.0-6.5 us |
| B shared, two writers per index | 2.2 us | 5.7-6.0 us | 15-19 us |
| C co-located | 1.0-1.1 us | 1.5-1.7 us | 1.8-4.8 us |

WORK=8 (~6 us per frame, closer to real codec cost), P=8: throughput B 1.27 vs C 1.29
G samples/s (shared-state overhead amortized); paced p99 B 9.7 us vs C 7.2 us, p99.9
22.8 vs 10.1 us; B with two writers per index p99 42 us, p99.9 115 us; A p99 1.2 ms.

Reading:
1. Co-location wins or ties everywhere and scales linearly.
2. Shared state costs 2x scaling when work per frame is light, and tails blow up when
   writers share an index for longer critical sections.
3. The cross-core handoff is the tail-latency risk. My handoff used busy-poll then
   `yield_now` with no pinning (macOS cannot pin) and 16 busy threads on 16 cores
   (4 efficiency cores). A Linux build with pinning and futex or eventfd wakeups
   should do much better: measured in "Linux rerun" below.

Limitations: synthetic work, no disk, no network, no reader consumption, macOS only,
2-second runs, one run for WORK=8.

## Linux rerun (2026-10-05)

Issue #9. Machine: AWS c7i.metal-24xl (bare metal), Intel Xeon Platinum 8488C, 1
socket, 48 cores, 1 NUMA node, Ubuntu 24.04, kernel 7.0.0-1013-aws, rustc 1.98.1.
Threads pinned with `core_affinity`, except design B, which `tpc-bench` does not pin.
Two runs, on two hosts of this type:

- **Baseline:** Q6's `tpc-bench`, unchanged. Its shards spin, then yield. They never
  park.
- **Wake path:** `bench/handoff` at b0fd43f (#469), `run.sh` with its defaults: 3 reps,
  10 s per run, P = 8, pins from core 1. Design A uses `ring`. Each shard runs one loop
  on a Tokio `LocalRuntime` that takes frames with `try_pop` and parks on all its
  rings when they are empty. A push wakes the loop through the runtime's driver
  (eventfd). The loop is the benchmark's own; `ring` is the production crate.
  `inline` is design C with no runtime: nothing crosses a thread.

```
bench/handoff/run.sh <host>
```

Raw output: [baseline](https://github.com/synnaxlabs/foundation/issues/9#issuecomment-5988122113),
[wake path](https://github.com/synnaxlabs/foundation/issues/9#issuecomment-6001162010).

Paced 10,000 frames/s per producer, P = 8 (80M samples/s). Ranges are over the runs.

| Design | WORK | p50 | p99 | p99.9 | Pops parked |
|---|---|---|---|---|---|
| A, spins (`tpc-bench`), S = 8 | 1 | 0.98-1.00 us | 1.24-1.27 us | 1.40-1.64 us | - |
| A, `ring` wake, S = 8 | 1 | 5.4-8.4 us | 8.8-9.5 us | 9.5-10.0 us | 100% |
| A, `ring` wake, S = 4 | 1 | 7.9-8.1 us | 9.9-10.2 us | 10.4-11.3 us | 75% |
| A, `ring` wake, S = 8, 20 us spin | 1 | 5.5-5.7 us | 9.7-11.3 us | 10.0-12.7 us | 75-100% |
| B shared (`tpc-bench`), not pinned | 1 | 0.99-1.02 us | 1.40-1.55 us | 1.77-1.94 us | - |
| C co-located (`tpc-bench`) | 1 | 0.61-0.62 us | 0.71-0.79 us | 0.97-1.17 us | - |
| C co-located (`inline`) | 1 | 0.54 us | 0.55-2.00 us | 0.57-2.95 us | - |
| A, spins (`tpc-bench`), S = 8 | 8 | 4.68 us | 4.96 us | 5.17-5.18 us | - |
| A, `ring` wake, S = 8 | 8 | 9.2-12.1 us | 13.2-15.1 us | 13.7-16.6 us | 88-100% |
| A, `ring` wake, S = 4 | 8 | 11.2-12.2 us | 15.9-23.3 us | 16.5-24.6 us | 50-63% |
| B shared (`tpc-bench`), not pinned | 8 | 4.67 us | 5.13-5.19 us | 5.33-5.44 us | - |
| C co-located (`tpc-bench`) | 8 | 4.30-4.32 us | 4.43-4.45 us | 4.59-4.65 us | - |
| C co-located (`inline`) | 8 | 4.23 us | 4.24-4.25 us | 4.46-6.15 us | - |

Unthrottled throughput, P = 8, S = 8 (G samples/s):

| Design | WORK = 1 | WORK = 8 |
|---|---|---|
| A, spins (`tpc-bench`) | 11.7-11.9 | 1.82 |
| A, `ring` wake | 9.5-10.1 | 1.74 |
| C co-located (`tpc-bench`) | 12.1 | 1.83-1.84 |
| C co-located (`inline`) | 13.6 | 1.87 |

Reading:
1. The wake through the runtime adds 4 to 7 us at p50 and 8 to 11 us at p99.9 over
   the spinning handoff, at S = 8 and both WORK values. Frames reach each shard once
   per 100 us tick (one frame at S = 8, two at S = 4), so the shard finds its rings
   empty and parks before most frames.
2. A 20 us spin window did not help: it ends before the next frame comes. A window
   that covers the gap is a busy core per shard, which is the spinning baseline.
3. There is no millisecond tail. Q6's risk for network frames does not occur on Linux
   with pinned threads: p99.9 stays under 25 us and the maximum under 0.4 ms.
4. Unthrottled, under 2% of pops park, and the handoff gives up 26 to 30% of
   throughput to `inline` at WORK = 1 and 7% at WORK = 8.

Recommendation for C2: lock the working assumption (decisions 1.5). A connector that
reads on its shard takes no handoff. Network data and frames from vendor threads take
one batched handoff, with one runtime wake per batch. This run woke once per frame, so 4
to 11 us is the cost of one wake, small against the P1 latest-mode budget of 250 us at
p99 over one LAN hop. Keep the spin window a per-node setting (performance rule 6). This
run supports a default of 0: a window helps only when it is longer than the gap between
frames, and then it keeps a core busy to save the wake. The sweep in decisions 5.3 sets
the default.

Limits:
- The `tpc-bench` and `bench/handoff` numbers come from two hosts of one type. The
  rerun of `tpc-bench` on the second host was lost.
- The run does not split the wake cost between the eventfd, the scheduler, and the
  exit of the parked core from its idle state.
- At S = 8, WORK = 1, the p50 of `ring` is 5.4 us or 8.4 us from one rep to the next.
- `inline` runs no runtime, so it is a lower floor than Q6's design C, which runs
  each shard on a `LocalRuntime`.
- One instance type, synthetic work, no disk, no network, no reader, no Pi 4.

## Risks

- Hot index saturates a core: needs an index ownership transfer protocol (design work,
  not yet specified).
- Handoff tails for network frames: measured on Linux with pinned threads and parking
  (Linux rerun). No millisecond tail; the wake adds 4 to 11 us.
- One iroh endpoint per node puts all QUIC work on one core: measure (fork 5).
- RefCell held across `.await` panics at runtime (Iggy): sans-I/O cores and a lint or
  review rule against borrows across await points.
- Disk I/O without io_uring on macOS and Windows: per-shard I/O thread or compio's
  driver; decide in S4.
- macOS cannot pin threads: macOS is a development platform; production numbers come
  from Linux.

## Decision to put to the user

Do you agree that work runs where its data lives: each core's shard owns its indexes
and also runs the connectors that write them, on one Tokio `LocalRuntime` per shard,
with network frames handed over once, blocking vendor calls on one thread per device,
and protocol cores written as sans-I/O state machines?
