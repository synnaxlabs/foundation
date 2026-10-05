# Research fork 11: memory, pooling, sharing, synchronization, queue sizing

Date: 2026-10-04. Scope: the user's request for "extremely deep research into memory
allocation, pooling, memory sharing ... synchronization, queue sizing", starting from the
Synnax Go frame bitmask. Inputs: `project_foundation_design.md` (S1, S2, B3, B4, P1, C1,
C2 open), `foundation-r1-thread-model.md`, `foundation-r2-storage.md`,
`foundation-r8-boundaries.md`. Benchmarks: `scratchpad/mem-bench/` (new Cargo project).

Marking rules: "(one source)" marks a claim with a single source. "(measured)" marks a
number from this fork's benchmarks on the Apple M3 Max. "(inference)" marks my own
reasoning with no source.

---

## 0. Summary

**The Go bitmask lesson, generalized.** The mask was fast because it removed two heap
allocations and a copy of every kept entry per frame per subscriber, which in Go also
removed garbage collector work. The deeper lesson is where the work sits: the mask still
does a hash lookup per entry per frame per subscriber. A writer sends the same key set
in every frame, so the filter result can be computed once per (key set, subscriber) and
reused. Measured in Rust: the cached mask lookup costs 4 to 5 ns per frame at any
frame size, versus 0.03 to 78 us for rebuilding vectors with UUID keys (section 6).

**Headline numbers (M3 Max, macOS, measured, medians of 3 to 5 runs, machine load 3 to 7
from other sessions):**

| What | Cost |
|---|---|
| Per-shard pool alloc+free, same thread | 1.0 to 1.1 ns |
| mimalloc 3.3 / snmalloc / jemalloc / macOS system, 64 B, same thread | 4.2 to 4.3 / 3.2 to 3.3 / 7.2 to 7.5 / 9.2 to 9.5 ns |
| Free on another thread, 4 KiB: pool with return ring / mimalloc / system | 6.2 to 7.2 / 60 to 73 / 69 to 130 ns |
| Shared lock-free pool (MPMC queue) under 2 / 8 threads | 75 to 132 / 1,177 to 1,306 ns per op |
| Arc clone+drop: uncontended / one Arc shared by 8 threads | 3.5 to 3.8 / 418 to 441 ns |
| Fan-out of one 64-series frame to 8 readers: copy + Arc per series / one Arc per frame + mask | 20.6 to 21.9 / 0.90 to 0.93 us |
| memcpy 4 KiB / 64 KiB | 60 ns / 0.9 us (68 to 74 GB/s) |
| SPSC ring handoff, busy-poll: one-way hop / per message, batched 64 | 49 to 77 ns / 0.57 to 0.76 ns |
| Wake a parked consumer (park/unpark, crossbeam, Tokio runtime): one-way p50 at 50 kHz / at 1 kHz | 5.5 to 6.5 us / 7.3 to 8.0 us (p99 8 to 24 us; p99.9 up to ms when cores are oversubscribed) |
| Tokio mpsc between two runtimes, throughput | 38 to 89 ns/msg vs 4.3 to 10.5 (rtrb) and 0.57 to 0.76 (rtrb batched) |
| Latest slot, 4 readers, writer at 100 kHz: seqlock read / write | 2.3 to 2.4 ns / 165 to 250 ns |
| Same with parking_lot Mutex: write | 2.6 to 2.8 us (readers starve the writer) |
| Frame filter, 1,024 entries keep 64: rebuild with UUID hash / mask with slot bitset / cached mask | 1,241 to 1,264 / 383 to 389 / 34 to 35 ns (incl. walking kept entries) |

**What the numbers say about the user's rule** ("minimize copying, locking, and heap
allocations at all costs"): the order of cost is wakeups (microseconds), then contended
atomics and shared queues (hundreds of nanoseconds), then allocator calls and per-series
refcounts (tens of nanoseconds), then copies of small and medium buffers (tens of
nanoseconds per 4 KiB on the M3, 25 times more on a Pi 4). A copy is cheap next to one
wakeup: one 6 us wake costs as much as 100 copies of 4 KiB on the M3. So the rule
should read: never wake per frame, never share a mutable cache line on the hot path,
never allocate or refcount per series, and fuse the copies we cannot avoid with work we
must do anyway (decode, decrypt, checksum).

**Shape-affecting findings (decide now; details in section 8):**

1. **Frames reference an interned key set**, not an owned `Vec<channel::Key>`. A key
   set is the sorted list of node-local channel slots (dense `u32`) that a writer
   session sends, created once per session. This revises S1's in-memory type, and it
   follows S1's own principle ("never put in a frame what both ends know").
2. **A filtered frame is a view** (frame reference plus a mask), and the mask is cached
   per (key set, subscriber). The home routes by key set: one lookup gives the list of
   interested readers with their masks. No broadcast-then-filter.
3. **A series is a slice of a shared, refcounted block**, and a frame's series share one
   block. One refcount per frame, never per series. This revises what S2's `Buffer`
   means.
4. **Pools are per shard, injected, and blocks go back to their owner shard** on release
   (Seastar, sharded-slab, mimalloc v3 all do this). No global pool (Iggy has one).
5. **Block layout uses offsets, never pointers**, so a block can later live in a shared
   memory segment for same-host SDKs without a format change.

Everything else (allocator choice, queue kinds and sizes, spin windows, batching, size
classes, refcount tricks) is a tunable internal, decided by benchmarks under T1.

---

## 1. The Synnax Go frame bitmask

### What it replaced

Before 2025-04-19, Synnax had three frame types: `cesium/internal/core.Frame`,
`synnax/pkg/distribution/framer/core.Frame`, and the storage `ts.Frame`, each a plain
`{Keys, Series}` pair. Filtering copied:

```go
// synnax/pkg/distribution/framer/core/frame.go before 38807420eb
func (f Frame) FilterKeys(keys channel.Keys) Frame {
    fKeys := make(channel.Keys, 0, len(keys))
    fArrays := make([]telem.Series, 0, len(keys))
    for i, key := range f.Keys {
        if keys.Contains(key) { // linear scan of the demanded slice
            fKeys = append(fKeys, key); fArrays = append(fArrays, f.Series[i])
        }
    }
    return Frame{Keys: fKeys, Series: fArrays}
}
```

The relay ran this for every frame and every subscriber, and called it twice per
frame (`relay/streamer.go:97` and `:102` at `38807420eb~1`): four heap allocations and
two O(frame keys x demanded keys) scans per subscriber per frame. Each layer crossing
also allocated a new key slice (`ToStorage`, `NewFrameFromStorage`).

### The change

- `38807420eb` (2025-04-19): one generic `telem.Frame[K]` replaces the three types. A
  filter on a frame with fewer than 128 entries sets bits in a `bit.Mask128`
  (`x/go/bit/mask.go`) instead of building new slices; larger frames still copy.
- `8317817b40` (same day): the first mask was a `*bit.Mask128`. Frames are passed by
  value, so two frames shared one mask and a filter on one changed the other. The
  commit added `Copy()` and turned the mask path off.
- `76929f53ad` (2025-04-22): the mask became a value, `struct { enabled bool;
  bit.Mask128 }` (17 bytes inside a 72-byte frame), with value receivers that return a
  new mask. No heap allocation, no aliasing. This is the shape in main today
  (`x/go/telem/frame.go:30-49`, filter at `:443-464`).
- PR #1220 (SY-2351, merged 2025-05-29) shipped it: "a new frame filtering mechanism to
  the stream pipeline that uses a bit-mask in order to reduce heap allocations", next to
  a new codec that "reduces server memory allocations and improves performance by close
  to 30%".
- Later: `UnsafeReinterpretKeysAs` (`frame.go:53`) converts key types between layers with
  no copy (`core/pkg/distribution/framer/frame/frame.go:84`, `:124`). PR #2861 (SY-4787,
  2026-09-04) replaced the per-entry linear scan with a cached `set.Set`: -62% geomean,
  -94.5% at 100 entries x 500 demanded keys. Its description notes that "routing at the
  delta instead of broadcast-then-filter" is still not done.

### Why it was fast

1. **No allocation per (frame, subscriber).** In Go every allocation feeds the garbage
   collector; the relay allocated two slices per subscriber per frame, sized by the
   frame. Local run of the Go benchmark today (`x/go/telem/frame_bench_test.go`, M3 Max,
   measured): mask path 52 ns at 10 entries to 541 ns at 100 entries, 0 allocations;
   copy path at 500 entries 4.3 to 8.0 us with 2 allocations and 4.3 to 43 KB per call.
2. **No copy of series headers.** A Go `Series` carries data type, time range,
   alignment, and a slice header; the copy path moved all of that per kept entry.
3. **Composition.** A second filter ORs into the same mask, and hot loops (codec,
   Cesium writer) walk raw slices with `ShouldExcludeRaw` and pay one bit test per entry.

### What it did not fix

- Per-entry lookups per frame per subscriber remain (set lookup since #2861). At 100
  entries and 50 demanded keys that is still 541 ns per subscriber per frame.
- Every streamer receives every frame and filters (broadcast-then-filter), so the cost
  is O(subscribers x entries) per frame.
- 128 entries is a hard limit; larger frames fall back to copying. (Nit: the guard is
  `len(f.keys) < 128`, so a frame of exactly 128 entries also copies.)
- `TrueCount` counts bits one at a time (`mask.go:31`) instead of a popcount.

### The lesson for Foundation

Rust has no collector, so an allocation costs less than in Go (4 to 15 ns with a good
allocator, measured). The same structure still wins in Rust, for refcount traffic and
cache misses instead of GC. The bigger lesson is to move work from "per frame" to "per
key set": section 6 measures 4 to 5 ns per frame for a cached mask at every frame size.

---

## 2. Allocation (item 1)

### 2.1 Global allocators by platform

| Allocator | Linux x86-64 | Pi 4 (ARM64, 4 KiB pages) | macOS | Windows | Notes |
|---|---|---|---|---|---|
| System | glibc ptmalloc: up to 8 x cores arenas on 64-bit, RSS growth with many threads ([Heroku](https://devcenter2.assets.heroku.com/articles/tuning-glibc-memory-behavior)) | glibc, same | libmalloc: slowest here, 9 to 545 ns (measured) | HeapAlloc (not measured) | Never on the hot path |
| mimalloc 3.x | yes | yes | yes | first-class (Microsoft) | v3 has no thread-local segments; free from another thread is one CAS on the page's concurrent free list ([v3 readme](https://raw.githubusercontent.com/microsoft/mimalloc/main3/readme.md)); purge delay 10 ms returns memory to the OS ([options.c](https://chromium.googlesource.com/external/github.com/python/cpython/+/ca878b6e45f9c7934842f7bb94274e671b155e09/Objects/mimalloc/options.c)); used by CPython free-threading |
| jemalloc 5.3 | yes | yes, but page size is a build setting: a 4 KiB build fails on 16 KiB kernels with "Unsupported system page size"; Pi 5 boots a 16 KiB kernel by default ([Arch ARM forum](https://archlinuxarm.org/forum/viewtopic.php?p=72450), [meta-raspberrypi](https://demo.gitea.com/rapgenic/meta-raspberrypi/commit/15967d6ad992f54ce9be53d82ff6a14eedefb49d)) | yes | weak | Upstream repo archived 2025-06-02; development continues in Meta's fork ([FreeBSD list](https://lists.freebsd.org/archives/cu/2025-June/007847.html)) |
| snmalloc 0.7 | yes | yes | yes | yes | Built for producer/consumer: remote frees return to the owner in batches by message passing ([ISMM 2019](https://www.microsoft.com/en-us/research/uploads/prod/2020/04/snmalloc.pdf)); BatchIt adds per-slab batching, over 20% on some producer/consumer workloads ([ISMM 2024](https://www.microsoft.com/en-us/research/uploads/prodnew/2024/05/preprint_batchit.pdf)) |

Measured on the M3 Max (ns per alloc+free pair, medians of 5, three runs; cross-thread
= producer allocates, consumer frees, through an SPSC ring that alone costs 17 to 25
ns/msg):

| Size | Pattern | system | mimalloc | jemalloc | snmalloc | per-shard pool |
|---|---|---|---|---|---|---|
| 64 B | same thread | 9.2 to 9.5 | 4.2 to 4.3 | 7.2 to 7.5 | 3.2 to 3.3 | 1.1 |
| 64 B | batch of 1,024 | 11.6 to 12.0 | 4.9 to 5.1 | 13.1 to 13.8 | 4.5 to 5.2 | 0.6 |
| 64 B | cross-thread | 25 to 33 | 20 to 29 | 38 to 46 | 11 to 17 | 4.0 to 4.4 |
| 4 KiB | same thread | 14.8 to 15.4 | 10.8 to 11.2 | 9.1 to 9.7 | 13.0 to 15.5 | 1.0 to 1.1 |
| 4 KiB | batch of 1,024 | 25 to 27 | 36 to 40 | 37 to 39 | 131 to 135 | 3.1 to 3.6 |
| 4 KiB | cross-thread | 69 to 130 | 60 to 73 | 71 to 84 | 56 to 70 | 6.2 to 7.2 |
| 64 KiB | same thread | 76 to 79 | 11.8 to 12.0 | 132 to 141 | 106 to 266 | 1.0 to 1.1 |
| 64 KiB | batch of 1,024 | 83 to 95 | 104 to 116 | 1,726 to 1,875 | 852 to 927 | 5.1 to 5.8 |
| 64 KiB | cross-thread | 475 to 573 | 108 to 122 | 320 to 368 | 171 to 429 | 19 to 21 |

Reading: allocators are fast for small, same-thread, steady-state work, and fall off a
cliff for large blocks and batches (page faults and returning memory to the OS: a batch
of 1,024 x 64 KiB is 64 MiB). A preallocated, pre-touched pool never faults. mimalloc is
the best general allocator here for large blocks; snmalloc for small cross-thread frees.

### 2.2 How the reference systems handle buffers

| System | Approach | Source |
|---|---|---|
| Seastar, Redpanda, ScyllaDB | At boot, take all memory and split it per core (NUMA-aware). Memory allocated on a core must be freed on that core; a cross-core free goes to the owner's list (`drain_cross_cpu_freelist`), and `foreign_ptr` sends destruction back to the owner core. | [Seastar memory docs](https://docs.seastar.io/master/namespaceseastar_1_1memory.html), [Redpanda tpc-buffers](https://redpanda.com/blog/tpc-buffers) |
| Redpanda iobuf | A refcounted chain of fragments, sizes 512 B growing about 1.5x to 128 KiB; other cores share a view with no copy, deletes deferred to the owner. | [Redpanda tpc-buffers](https://redpanda.com/blog/tpc-buffers) (one source) |
| TigerBeetle | All memory allocated at startup from configured limits ("a bit of addition and multiplication"); no allocation after; over-limit connections are dropped. Benefits: no OOM, predictable latency, no use-after-free class. | [TigerBeetle blog](https://tigerbeetle.com/blog/2022-10-12-a-database-without-dynamic-memory/) |
| LMAX Disruptor | Ring of entries created once at startup and reused; power-of-two size; single producer about 80M ops/s vs 28M multi-producer. | [Disruptor user guide](https://lmax-exchange.github.io/disruptor/user-guide/index.html) |
| Aeron | Log buffers are memory-mapped files (three term partitions); publishers write in place, subscribers read in place; IPC uses the same files between processes. | [Aeron flow control wiki](https://github.com/real-logic/aeron/wiki/Flow-And-Congestion-Control) |
| DPDK mempool | Fixed-size objects in a ring, plus a per-core cache with no locks (max 512 entries by default) that refills and spills in bulk; mbufs are refcounted; objects padded so start addresses spread over memory channels. | [DPDK mempool guide](https://doc.dpdk.org/guides/prog_guide/mempool_lib.html), [DPDK bug 1027](https://mails.dpdk.org/archives/dev/2022-June/243520.html) |
| io_uring | Registered (fixed) buffers are mapped into the kernel once, not per I/O, which pays off with O_DIRECT; zero-copy receive (Linux 6.15) DMAs straight into user pages with header/data split hardware. | [man io_uring_register_buffers](https://man7.org/linux/man-pages/man3/io_uring_register_buffers.3.html), [kernel zcrx docs](https://docs.kernel.org/6.15/networking/iou-zcrx.html) |
| Apache Iggy | One global pool (`OnceLock` static) of 4 KiB-aligned buffers in 28 size buckets from 4 KiB to 512 MiB, each bucket a shared `crossbeam::ArrayQueue`; default 4 GiB, minimum 512 MiB; flume channels between shards; compio boxes every I/O request and relies on mimalloc. | `apache/iggy` `core/server_common/src/memory_pool.rs`, `core/server/config.toml:439-450`; [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/) |

Two lessons for Foundation: (1) everyone who cares about tails keeps buffers per core
and returns them to the owner; (2) Iggy's shared atomic queues are the slow pattern in
our measurements (75 to 1,306 ns per op under contention), and its 512 MiB minimum does
not fit a Pi with a 50 MB idle budget.

### 2.3 What "zero allocation on the hot path" takes in Rust

1. **Pool blocks for sample data**, per shard, preallocated and touched by the owning
   shard thread (first touch places pages on that core's NUMA node:
   [set_mempolicy(2)](https://www.man7.org/linux/man-pages/man2/set_mempolicy.2.html)).
2. **No per-frame `Vec`, `Box`, `String`, or map growth.** Key sets and masks are
   interned and cached (section 6). Scratch vectors keep their capacity (`clear()`).
   Small arrays inline (`SmallVec`).
3. **No `bytes::Bytes` as the internal block type.** A `Bytes` is 32 bytes, every clone
   is an indirect call plus an atomic add, the first clone of a `Vec`-backed `Bytes`
   allocates (measured 7.4 to 8.3 ns vs 3.8 ns for later clones), and `Bytes::from_owner`
   boxes the owner, so wrapping a pool block allocates once (`bytes-1.12.1/src/bytes.rs:254`).
   The vtable is crate-private, so a zero-allocation `Bytes` over our pool is not
   possible. Convert only at the transport edge where an API demands `Bytes`.
4. **Encryption is the one copy we always pay on send.** quinn builds each packet by
   writing stream data into the packet buffer and then encrypts in place
   (`quinn-proto/src/crypto.rs`: `fn encrypt(&self, packet: u64, buf: &mut [u8], ..)`).
   On receive, a series that spans packets must be assembled; the decode or the
   assembly is that copy. Fuse it with decoding into the destination block.
5. **Refcount once per frame, not per series**, and never on a line many cores write
   (section 3).
6. **Free on the owner.** Measured: a 4 KiB block freed on another thread costs 60 to
   130 ns through malloc vs 6.2 to 7.2 ns through a pool with a return ring.
7. **No per-frame `spawn`, `oneshot`, boxed error, or log formatting.** Tokio boxes each
   spawned future; a oneshot allocates its shared state (inference from the Tokio
   source layout, not measured here).
8. **Enforce it with tests.** A test allocator that fails on allocation inside a marked
   region ([assert_no_alloc](https://github.com/Windfisch/rust-assert-no-alloc)) or exact
   allocation counts ([dhat-rs heap testing](https://github.com/nnethercote/dhat-rs)).
   This belongs in T1 layer 4 and the C9b2 performance agent's rulebook.

The custom-allocator API (`allocator_api`) is still nightly; the Rust project lists
"Allocators 1.0" as a 2026-2027 goal ([Rust goals](https://rust-lang.github.io/goals/2026/allocators-1.0.html)).
Foundation's pool must not depend on it: pool blocks are their own type, not `Vec<u8,
A>`.

### 2.4 Recommendation

- `#[global_allocator]` = mimalloc 3 in `node`, for everything off the hot path (one
  line; a tunable, benchmark again on Linux and Pi).
- Sample data lives only in per-shard pool blocks. jemalloc is ruled out for one ARM64
  binary across Pi 4 (4 KiB pages) and Pi 5 (16 KiB pages) unless built for 64 KiB
  pages; mimalloc and snmalloc detect the page size at run time (inference from their
  docs; verify on a Pi 5).
- Pool memory is reserved up front but committed on demand and purged when idle, sized
  from settings (TigerBeetle-style limits, not TigerBeetle-style full commit), so a Pi
  stays under 50 MB idle.

---

## 3. Memory sharing (item 2)

### 3.1 One buffer, many readers, no copies

| Technique | Read cost | Write or reclaim cost | Fit |
|---|---|---|---|
| Refcounted immutable block (`Arc`-like) | one atomic add and sub per holder | free when the count hits zero | The data path. One count per frame. |
| Biased refcount: owner thread uses a plain counter, others an atomic one ([Choi et al., PACT 2018](https://www.springerprofessional.de/doi/10.1145/3243176.3243195); CPython 3.13 free-threading, [LWN](https://lwn.net/Articles/872869)) | non-atomic for the owner shard | merge rule on the owner's last drop | Optimization for later; measure first |
| Epoch-based reclamation (crossbeam-epoch) | free on the read side | a stalled thread blocks all reclamation | Not needed: shards own state |
| Hazard pointers | a fence per protected load | prompt, bounded | Not needed on the data path |
| arc-swap (hazard-pointer-like "debts") | 7 to 21 ns load (measured) | store pays all debts: 240 to 1,078 ns (measured) | Read-mostly snapshots: `Arc<spec::Tree>` (R8 1.9) |
| Ownership transfer (move the handle, free on owner) | none | one queue push back to the owner | Pool blocks across shards |

Measured refcount costs: `Rc` 2.6 to 2.8 ns, `Arc` 3.5 to 3.8 ns, `Bytes` 3.8 to 3.9
ns per clone+drop on one thread. One `Arc` cloned and dropped by 2 / 4 / 8 threads: 17
to 18 / 61 to 71 / 418 to 441 ns per pair per thread, against 3.6 to 5.4 ns with
private `Arc`s. Contention, not atomics, is the cost.

Fan-out measured (one producer, readers on their own threads, SPSC rings, ns per frame on
the producer):

| Readers | Series | Copy series list + `Arc` per series | `Arc<Frame>` + mask |
|---|---|---|---|
| 1 | 8 | 217 to 281 | 28 to 34 |
| 4 | 8 | 1,150 to 1,331 | 299 to 352 |
| 4 | 64 | 7,106 to 7,605 | 297 to 382 |
| 8 | 64 | 20,592 to 21,909 | 895 to 932 |

Per-series refcounts multiply the contended line count by the series count: every
reader decrements the same 64 counters. One count per frame is 18 to 26 times cheaper
at 64 series.

### 3.2 Cross-core cache effects

- **False sharing.** Two unrelated atomics on one line slow each other about 10x on
  x86-64 and less on Apple M1 ([Mara Bos, Rust Atomics and Locks ch. 7](https://mara.nl/atomics/hardware.html)).
  Pad producer and consumer indices, refcounts, and per-shard counters to 128 bytes:
  Apple M-series lines are 128 bytes (`sysctl hw.cachelinesize` = 128 on this machine),
  and Intel's spatial prefetcher pulls pairs of 64-byte lines
  ([crossbeam CachePadded](https://docs.rs/crossbeam-utils/latest/src/crossbeam_utils/cache_padded.rs.html)).
  Pi 4 (Cortex-A72) lines are 64 bytes; padding to 128 wastes only memory.
- **Contended lines.** Measured: shared MPMC pool 75 to 132 ns at 2 threads, 1.2 to 1.3
  us at 8. Shared `Arc` 418 to 441 ns at 8 threads.
- **NUMA.** Single-socket edge machines and the Pi have one node. Large cloud VMs can
  have several; Linux places a page on the node of the thread that first touches it
  ([set_mempolicy(2)](https://www.man7.org/linux/man-pages/man2/set_mempolicy.2.html)),
  so each shard initializes its own pool. Seastar splits memory per NUMA node at boot.

### 3.3 Sharing with SDK processes on the same host

- **Aeron IPC** maps the same log buffer file into both processes; publishers claim and
  write in place; flow control is by positions (section 5).
- **iceoryx2** (Rust core, 0.10.0 on 2026-09-18) is decentralized with no daemon, has
  no system calls in the delivery path, supports Linux, macOS, Windows, FreeBSD, QNX,
  and has C, C++, and Python bindings ([docs.rs](https://docs.rs/crate/iceoryx2/latest),
  [ROSCon 2024 talk](https://roscon.ros.org/2024/talks/iceoryx2__A_Journey_to_Becoming_a_First-Class_RMW_Alternative.pdf)).
  It claims sub-microsecond latency independent of size (vendor claim, not checked).
  Pre-1.0, so a reference design, not a dependency today (library rule).
- **Trust.** A process that shares writable memory with an untrusted peer must copy
  before it checks, or the peer can change the bytes after the check (TOCTOU). Linux
  memfd seals (`F_SEAL_WRITE`) remove the need, but only on Linux
  ([memfd sealing patch](https://lkml.rescloud.iu.edu/1404.1/04758.html)). So: node to
  SDK (SDK maps read-only) can be zero-copy; SDK to node needs one copy, fused with the
  home's validation (R8 Q4), except on Linux with sealed memfds.
- **Python.** A series in shared memory is a NumPy array with no copy
  (`np.frombuffer` over the mapping). The node must then keep the block alive until the
  SDK releases it, which needs a loan and return protocol (iceoryx2 does this).

**Recommendation:** do not build a shared-memory transport now. Make the block layout
position-independent (offsets, never raw pointers, inside a block) so it can move into
a shared segment later with no format change. The SDK uses the wire protocol over a
local socket first (C7).

---

## 4. Synchronization (item 3)

### 4.1 Queues

Measured, M3 Max, threads unpinned (macOS cannot pin), four runs at machine load 3 to
8 from other sessions:

| Handoff | One-way hop, ping-pong | Throughput, ns/msg (cap 1,024) |
|---|---|---|
| rtrb SPSC, both busy-poll | 49 to 77 ns | 4.3 to 10.5 one at a time; 0.57 to 0.76 in chunks of 64 |
| rtrb SPSC, spin 10,000 then park | 52 to 77 ns (stays hot) | n/a |
| rtrb SPSC, park at once | 1.3 to 2.6 us | n/a |
| crossbeam bounded (spins, then parks) | 80 to 115 ns | 9.6 to 12.2 |
| std `sync_channel` | 1.5 to 2.7 us | 5.6 to 6.4 |
| tokio mpsc, two current-thread runtimes on two threads | 2.7 to 3.3 us | 57 to 84 (recv) / 38 to 89 (`recv_many` 256) |
| tokio mpsc, two tasks on one multi-thread runtime | 61 to 66 ns | n/a |

One-way latency, one message every 20 us (50 kHz), 100,000 messages, us. Typical runs
(1 and 4) and the run at load 8.4 (3):

| Consumer | p50 | p99 | p99.9 | max | Run 3 p50 / p99 |
|---|---|---|---|---|---|
| rtrb, busy-poll | 0.08 to 0.12 | 0.42 to 2.9 | 6.5 to 9.5 | 22 to 398 | 0.08 / 0.33 |
| rtrb, spin 10,000 then park | 0.08 to 0.17 | 0.29 to 3.3 | 3.7 to 7.2 | 24 to 44 | 0.08 / 1,964 |
| rtrb, spin 100 then park | 5.5 to 5.8 | 9.5 to 11.5 | 25 to 2,595 | 60 to 4,069 | 101 / 2,664 |
| crossbeam bounded | 5.5 to 5.8 | 8.1 to 14.2 | 13 to 47 | 128 to 583 | 1,256 / 9,454 |
| tokio mpsc into a current-thread runtime | 5.7 to 6.0 | 8.8 to 14.1 | 12 to 3,854 | 51 to 5,837 | 6.5 / 10.8 |

One message every 1 ms (1 kHz, the control-loop case), 5,000 messages, us: busy-poll
p50 0.29, p99 0.75; spin 10,000 then park p50 7.3, p99 14; spin 100 then park p50 8.0,
p99 24; crossbeam p50 8.0, p99 23; Tokio p50 7.7, p99 19. Every parking consumer had a
p99.9 of 0.87 to 3.2 ms at that machine load.

Reading:
- A cache-line handoff between cores costs 50 to 115 ns. Waking a sleeping thread costs
  1.3 to 2.7 us per hop in ping-pong and 5.5 to 8 us one-way after a real sleep on
  macOS. Linux reports 5 to 10 us to wake a thread on a remote idle CPU
  and 1 to 1.5 us on the same CPU ([UMCG LKML post](https://lkml.rescloud.iu.edu/2105.2/09667.html), one source).
- Tokio mpsc across runtimes is 4 to 20 times slower per message than an SPSC ring and
  50 to 150 times slower than a batched ring, because each send may wake the other
  runtime's driver. Fine for control messages, not for frames.
- Batching takes the per-message cost from 4.3 to 10.5 ns down to 0.57 to 0.76 ns.
  Wakes must be per batch.
- R1's handoff tails (p99 0.18 to 1 ms) came from busy-poll plus `yield_now` with 16
  busy threads on 16 cores. With a parked consumer on a lightly loaded machine the p99
  here is 8 to 24 us. On an oversubscribed machine every parking design showed
  millisecond p99.9 tails, and only busy-poll stayed under 10 us: a wake depends on the
  scheduler finding a core. This must be measured on Linux with pinned shards before
  the P1 latest-mode budget (p99 under 250 us) is trusted.
- A spin window only helps when it is longer than the gap between messages. At 1 kHz a
  10,000-iteration spin still parks (p50 7.3 us); keeping a 1 kHz loop hot costs a
  whole core.

**A finding about wake protocols.** My first spin-then-park queue hung on the M3: the
consumer set `sleeping = true` (SeqCst store) and then re-checked the ring, whose
acquire load compiles to `LDAPR` on Apple silicon (`rustc --print cfg` shows `rcpc`;
the binary has 278 `ldapr`). `LDAPR` may pass an earlier `STLR`, so both sides missed
each other and the consumer slept forever. A SeqCst fence on both sides fixes it. The
same code would likely pass on x86-64 (a SeqCst store is `xchg`) and on a Pi 4 (no
RCpc, so acquire is `LDAR`), so this class of bug escapes Linux CI. Every wake protocol
needs model checking (loom or shuttle) in T1 layer 1, and tests on Apple silicon or
Graviton.

### 4.2 Lock-free vs locks under contention

- Lock-free is not free: a shared lock-free queue as a pool free list took 75 to 132 ns
  per op at 2 threads and 1.2 to 1.3 us at 8 (measured). The cost is the shared line,
  not the lock.
- Under heavy contention a good mutex beats a spinlock 5 to 6 times
  (parking_lot 10 ms vs spin 55 ms, 32 threads), because a sleeping queue keeps only
  one waiter awake ([matklad](https://matklad.github.io/2020/01/04/mutexes-are-faster-than-spinlocks.html), one source).
- The way out is no sharing: one owner per piece of state (C2 shards), SPSC rings
  between pairs (Seastar keeps one 128-entry SPSC queue per core pair and flushes in
  batches of 16: `seastar/include/seastar/core/smp.hh:179-187`).

### 4.3 Latest-value slot (B4)

Measured (ns per read per reader, ns per write; "torn" = reads with mixed words, 0 in
every case):

| 64-byte value | 1 reader, writer flat out | 4 readers, flat out | 1 reader, writer 100 kHz | 4 readers, writer 100 kHz |
|---|---|---|---|---|
| seqlock | 129 to 158 / 8 to 10 | 452 to 526 / 17 to 20 | 2.3 / 95 to 98 | 2.3 to 2.4 / 165 to 250 |
| parking_lot Mutex | 47 to 121 / 5 to 6 | 120 to 135 / 12 to 13 | 7.3 to 7.5 / 189 to 286 | 63 to 66 / 2,591 to 2,763 |
| parking_lot RwLock | 1,019 to 1,584 / 5 | 378 to 439 / 10 to 11 | 7.5 to 7.6 / 140 to 189 | 37 to 47 / 273 to 549 |
| ArcSwap (load, copy) | 19 to 21 / 243 to 258 | 13 to 14 / 775 to 947 | 6.8 to 6.9 / 326 to 373 | 6.8 to 6.9 / 809 to 1,078 |
| triple_buffer (one reader) | 7.0 to 7.2 / 55 to 58 | n/a | 1.8 to 2.0 / 64 to 78 | n/a |

512-byte value, 4 readers, writer flat out: seqlock reads 5.3 to 6.3 us (readers retry
while the writer rewrites), Mutex 269 to 304 ns.

Reading:
- A seqlock is the fastest reader when writes are rare, and it starves readers when
  writes are frequent and values are large. The Linux kernel documents the same reader
  starvation ([kernel seqlock docs](https://docs.kernel.org/_sources/locking/seqlock.rst.txt)).
- A correct seqlock in Rust needs atomic word reads; reading the data with plain or
  volatile loads is undefined behavior under the Rust memory model until a bytewise
  atomic memcpy exists (RFC 3301, [UCG discussion](https://internals.rust-lang.org/t/include-racy-reads-in-rust-memory-model-with-maybeinvalid-t/24289)).
  My version uses `AtomicU64` words.
- Mutex readers starve the writer at 4 readers (2.6 to 2.8 us per write).
- B4's slot holds a frame handle, not a value. The right shape is a depth-1 mailbox:
  the shard swaps in the newest frame handle and gets the old one back to release; the
  reader swaps it out. That is the triple-buffer idea with handles (one atomic swap per
  side, no retries, no allocation). Readers on the home's own shard need no atomics at
  all (C2).

### 4.4 Atomic ordering costs, ARM vs x86

- x86-64: loads and stores are already acquire and release; only a SeqCst store differs
  (`xchg`). ARM64: acquire and release use `LDAR`/`STLR` (or `LDAPR` with RCpc); RMW
  operations use LL/SC loops on ARMv8.0 and single LSE instructions on ARMv8.1+
  ([Mara Bos ch. 7](https://mara.nl/atomics/hardware.html)).
- The Pi 4's Cortex-A72 is ARMv8.0 with no LSE. Rust enables outline atomics on
  aarch64 Linux since 1.57, so one binary uses LSE where present and LL/SC on the Pi 4;
  LSE can be up to 10 times faster under contention ([AWS Graviton Rust guide](https://aws.github.io/graviton/rust.html)).
  So contended atomics hurt most on the Pi. Rerun `arc` and `alloc` there.

### 4.5 Wakeups, busy-poll vs sleep

- Seastar busy-polls idle for 200 us on bare metal and 2 ms in VMs before it sleeps,
  and offers `--overprovisioned` (poll time 0) for laptops and containers
  (`seastar/src/core/reactor.cc:5004-5012`, `:4053`).
- LMAX offers Blocking, Sleeping, Yielding, and BusySpin waits, busy-spin only when
  handler threads are fewer than physical cores ([Disruptor guide](https://lmax-exchange.github.io/disruptor/user-guide/index.html)).
- A Pi 4 draws about 2.1 W idle and 6.4 W at full load ([Raspberry Pi thermal testing](https://www.raspberrypi.com/news/thermal-testing-raspberry-pi-4/));
  one spinning core of four is about a quarter of that span (inference) and heats a
  passively cooled box.
- At P1's 100M samples/s, one wake per frame of 1,000 samples is 100,000 wakes/s, or
  0.2 to 0.8 cores of pure wake cost (inference from the measured 2 to 8 us). Wakes
  must be per batch, and an active shard should poll its rings in its loop, not wait
  on each.

**Recommendation:** every queue boundary has one wake policy: spin for a window, then
park, with "notify only if the consumer announced sleep" plus fences on both sides
(model-checked). The window is a setting per node (default about 50 to 200 us on
servers, 0 on a Pi or when the node is overprovisioned), tuned by T1 benchmarks.

---

## 5. Queue sizing and backpressure (item 4)

### 5.1 Little's law per queue

Occupancy L = arrival rate x time in the queue (λ x W). Size each queue for the
longest stall it must absorb, in the unit it is limited by (bytes for memory and links,
entries for rings). Examples (inference, numbers to be replaced by measurements):

| Queue | λ | W (stall to absorb) | L | Starting size |
|---|---|---|---|---|
| Device actor -> shard (SPSC, frame handles) | 1,000 frames/s per device | 10 ms (flush burst) | 10 frames | 64 entries |
| Network shard -> owning shard (SPSC per pair) | 100,000 frames/s per node / 8 shards | 1 ms | 13 frames | 128 entries, batches of 16 (Seastar) |
| Disk group commit (B5: full = gap) | 800 MB/s (100M samples x 8 B) | fsync 2 to 10 ms (NVMe), 10 to 100 ms (SD card) | 1.6 to 8 MB; 80 KB to 4 MB on a Pi at 1M samples/s | settings, from the disk's measured sync time |
| Complete reader in flight (B3 credits) | link rate | RTT | BDP: 1 Gbit/s x 50 ms = 6.25 MB; 100 Mbit/s x 50 ms = 625 KB | bytes, adaptive |
| Latest reader (B4) | any | any | 1 | 1 (by definition) |
| Block return to owner | frames released by other shards | owner's loop period | blocks in flight | at least the pool's share lent out |

### 5.2 Credits (B3)

- Credits should count bytes. Memory, links, and disks are limited in bytes; frames and
  samples vary in size. quinn's default stream window is the BDP of 100 Mbit/s x 100 ms
  = 1.25 MB, and its send window 8 times that (`quinn-proto/src/config/transport.rs:375-406`).
  Aeron lets a publisher run ahead of a subscriber by the publication window (half the
  term, term default 16 MB) plus the receiver window (default 128 KB)
  ([Aeron wiki](https://github.com/real-logic/aeron/wiki/Flow-And-Congestion-Control)).
  gRPC estimates BDP with pings and grows its window to match
  ([gRPC-Go blog](https://grpc.io/blog/grpc-go-perf-improvements/)).
- B3's "one cumulative number per index" works as the acknowledgment; the window is the
  number of bytes beyond it the home may send. Start from the link's BDP and adapt
  (gRPC style). Starlink and cellular RTTs move; a fixed window would waste either
  memory or throughput.

### 5.3 Batching vs latency

- Smart batching (send what is queued as soon as the link is free; no timer) lowers
  mean and worst-case latency under bursts: 10 messages at 100 us each, average 500 us
  serial vs 150 us batched ([Thompson 2011](https://mechanical-sympathy.blogspot.com/2011/10/smart-batching.html)).
  B6 already chose this.
- Kafka moved `linger.ms` from 0 to 5 in 4.0 (KIP-1030) because one record per batch
  wasted requests ([KIP-1030](https://cwiki.apache.org/confluence/display/KAFKA/KIP-1030:+Change+constraints+and+default+values+for+various+configurations)).
  For Foundation, a linger above zero is a per-link setting (B6), never the latest-mode
  default.

### 5.4 What sizes others use

| System | Queue or window | Size | Why |
|---|---|---|---|
| Seastar | SPSC per core pair | 128 entries, batch 16 | Bounded cross-core work, amortized flushes |
| Disruptor | Ring | power of two, chosen by user | Index by mask |
| DPDK | Per-core mempool cache | up to 512 objects | Bulk refill and spill |
| Aeron | Term / publication window / receiver window | 16 MB / term/2 / 128 KB | Positions bound how far a publisher leads |
| quinn | Stream / connection send window | 1.25 MB / 10 MB | BDP of 100 Mbit/s x 100 ms |
| HTTP/2 | Initial stream window | 65,535 bytes (RFC 9113) | Conservative start; gRPC grows it by BDP |
| Kafka producer | `batch.size` / `linger.ms` | 16 KiB / 5 ms (4.0) | Fewer, larger requests |
| Tokio mpsc | Blocks inside the channel | 32 slots per block (`tokio/src/sync/mpsc/mod.rs:140`) | Linked blocks, user capacity |
| Iggy | Pool buckets | 4 KiB to 512 MiB, 8,192 per bucket, 4 GiB total | Server-class memory |
| TigerBeetle | Every queue | explicit limits from config | Static allocation |

### 5.5 Tunable and measurable

- Each queue reports, as status channels under its node name (S8): capacity, depth
  high-water mark, time in queue (histogram), full events, and gaps written (B5). Time
  in queue is the signal to act on, not length: CoDel controls on sojourn time (target
  5 ms, interval 100 ms) because length means nothing without the drain rate
  ([CoDel draft](https://datatracker.ietf.org/doc/html/draft-ietf-aqm-codel)).
- Sizes come from formulas over settings (rate x budget), not constants, so a Pi and a
  server get different numbers from the same code.
- T1 layer 6 sweeps the sizes and spin windows and keeps the defaults that win (B6
  already says this for transmission).

---

## 6. Frame data structure (item 5)

### 6.1 Measurements

One subscriber, universe of 100,000 channels, ns per frame including walking the kept
entries (medians of 5, three runs; ranges across runs):

| Frame entries | Kept | Demanded | Rebuild, UUID hash | Rebuild, slot bitset | Mask, UUID hash | Mask, slot bitset | Mask, sorted merge | Cached mask per key set | Cache lookup only |
|---|---|---|---|---|---|---|---|---|---|
| 8 | 4 | 54 | 33 to 36 | 28 to 29 | 15 to 16 | 8 | 42 to 44 | 4 | 4 to 5 |
| 64 | 32 | 532 | 233 to 238 | 191 to 195 | 98 to 99 | 55 to 56 | 374 to 384 | 17 to 18 | 4 to 5 |
| 128 | 16 | 1,016 | 203 to 211 | 143 to 147 | 184 to 191 | 74 to 75 | 732 to 744 | 10 | 4 to 5 |
| 1,024 | 64 | 1,064 | 1,241 to 1,264 | 747 to 753 | 1,377 to 1,407 | 383 to 389 | 1,996 to 2,053 | 34 to 35 | 4 to 5 |
| 1,024 | 1,024 | 2,024 | 5,972 to 6,089 | 4,635 to 4,714 | 1,965 to 1,996 | 1,012 to 1,088 | 2,561 to 2,712 | 650 to 684 | 4 to 5 |
| 10,000 | 100 | 1,100 | 10,656 to 10,917 | 5,621 to 5,728 | 14,753 to 15,076 | 3,530 to 4,818 | 8,771 to 9,287 | 149 to 154 | 4 to 5 |
| 10,000 | 10,000 | 11,000 | 75,794 to 77,724 | 56,204 to 57,777 | 23,487 to 23,820 | 9,988 to 10,544 | 24,398 to 24,732 | 6,316 to 6,480 | 5 |

Split one frame by home (ns per frame, rebuild / masks / cached): 64 entries into 2
homes: 456 to 465 / 426 to 432 / 12; 1,024 into 4: 6,127 to 6,275 / 1,478 to 1,501 / 19;
10,000 into 8: 55,984 to 56,504 / 14,080 to 14,162 / 34 to 35.

Reading:
- UUID keys cost 1.2 to 4 times more than dense `u32` slots for the same algorithm
  (hashing 16 bytes vs one bit test in a 12.5 KB bitset that fits L1).
- A mask beats rebuilding by 2 to 8 times; rebuilding also bumps one refcount per kept
  series.
- Caching per key set makes the filter itself O(1): 4 to 5 ns at every size. What
  remains is the reader walking its kept entries, which it must do anyway.
- Masks per home make a split 4 times cheaper than rebuilding; cached masks make it
  1,600 times cheaper at 10,000 entries.

### 6.2 Prior art for each trick

- **Shared schema reference per batch.** Arrow's `RecordBatch` is `{ schema: SchemaRef
  (Arc<Schema>), columns, row_count }`, so batches of one stream share one schema
  ([arrow-rs source](https://docs.rs/arrow-array/latest/src/arrow_array/record_batch.rs.html)).
- **Cached match results.** NATS caches subject match results (up to 1,024 subjects,
  swept to 256) so a publish skips wildcard matching ([NATS sublist](https://git.taigrr.com/gogrlx/nats-server/src/branch/main/server/sublist.go)).
- **Short numbers for keys.** Sparkplug B aliases bind a `uint64` to a metric name at
  birth and use it in every later message (`scratchpad/sp.txt:2361`); MQTT 5 topic
  aliases; Synnax's codec negotiates the key list per session and sets an
  "all channels present" flag to send no keys at all
  (`core/pkg/distribution/framer/codec/codec.go:338-372`). A4 already says the wire
  swaps keys for short numbers per connection.
- **Zero-copy slices of shared buffers.** Arrow `Buffer::slice` is O(1) over an
  `Arc`-owned allocation ([arrow-buffer docs](https://docs.rs/arrow-buffer/58.3.0/arrow_buffer/buffer/struct.Buffer.html));
  Redpanda iobuf shares fragments across cores.

### 6.3 Proposed in-memory shape (refines S1 and S2)

```rust
// types (layer 1): values only
pub mod channel {
    pub struct Key(Uuid);   // A4, unchanged: identity in spec, wire setup, disk footers
    pub struct Slot(u32);   // node-local dense number, assigned when the node learns
                            // the channel; never sent on the wire; never reused while
                            // any key set holds it
}

/// An interned, immutable, sorted list of slots. Every frame of one writer session
/// points at the same key set. Created at writer open, not per frame.
pub struct KeySet { id: u32, slots: Box<[channel::Slot]> }

/// One refcounted pool block holds the frame header, one descriptor per series, and
/// the series bytes back to back. Offsets only (no pointers) inside the block.
pub struct Frame(BlockRef);            // 8 bytes; clone = one atomic add
//   header:      key set id, entry count, sample count per index group (R2 Q1)
//   descriptors: [ { seq: u64, offset: u32, len: u32 }; n ]   16 bytes each
//   data:        series bytes, each aligned to 8 (or 64 for SIMD codecs)

/// What a reader receives: the frame plus which entries are its own.
pub struct View { frame: Frame, mask: Mask }   // Mask: inline u128 up to 128
                                                // entries, else a cached shared mask
```

Operations and their cost:

| Operation | How | Cost per frame |
|---|---|---|
| Write (connector) | `hub::block(len)` gives a block; the connector writes descriptors and data | no allocation, no refcount |
| Filter for a reader | `route[key set id]` gives `[(reader, mask)]`, built once per key set and reader | one lookup + one handle clone per interested reader |
| Split by home | `partition[key set id]` gives one mask per home | same |
| Merge (B6 catch-up) | chain views; the codec writes consecutive frames with one key set once | no copy in memory |
| Latest slot (B4) | swap the view into the reader's depth-1 mailbox | one atomic swap cross-thread, none on the same shard |
| Release | last holder returns the block to its owner shard | one push on the owner's return queue |

At 100,000 channels the per-frame cost depends on the number of interested readers, not
on the channel count or the frame size. That is what P1's "within 2x at 100k channels"
needs: many small frames (an OPC UA connector at 10 Hz across 100k tags is up to 1M
frames/s) make per-frame overhead, not per-sample overhead, the limit (inference).

Costs and risks:
- A key set table and a slot table per node, kept in step with the spec (S9). A spec
  change that adds channels to a writer's group starts a new key set; old frames keep
  the old one until released.
- The route cache must be cleared when a reader's selector gains channels (B2 live
  patterns). One generation number per reader suffices.
- A frame whose data outgrows one block needs a larger size class or a chain. Choose
  size classes by benchmark (tunable).
- Small frames waste block space. A latest slot that holds one 8-byte sample must not
  pin a 4 KiB block: size classes start small (for example 256 B), or the latest slot
  copies tiny frames into a compact slab (tunable).

---

## 7. Benchmarks (item 6)

Project: `scratchpad/mem-bench/` (standalone Cargo project, outside the repo). Binaries:
`alloc`, `arc`, `handoff`, `latest`, `filter`, `copy`. Raw outputs: `run-*.txt`.

Machine: Apple M3 Max (12 performance + 4 efficiency cores, 128-byte lines, 128 KiB
L1D), 48 GB, macOS 27.0.1. rustc 1.98.1, release, thin LTO, 1 codegen unit. Crates:
mimalloc 0.1.52 (bundles mimalloc 3.3.2), tikv-jemallocator 0.6.1 (jemalloc 5.3),
snmalloc-rs 0.3.8, rtrb 0.4.0, crossbeam-channel 0.5.17, tokio 1.53.2, arc-swap 1.9.2,
triple_buffer 8.1.1, parking_lot 0.12, bytes 1.12.1, smallvec 1. The Go numbers ran
`x/go/telem/frame_bench_test.go` with Go 1.27.1.

Commands:

```
cd scratchpad/mem-bench && cargo build --release
N=3000000 ./target/release/alloc
N=10000000 ./target/release/arc
N=400000 PACED=100000 TPUT=10000000 ./target/release/handoff   # PERIOD_US=1000 for 1 kHz
MS=700 ./target/release/latest
ITERS=100000 ./target/release/filter
./target/release/copy
```

Limitations:
- macOS cannot pin threads; other sessions kept the load average at 3 to 7 during the
  runs. Cross-thread numbers vary up to 2x between runs; same-thread numbers within
  about 10%. Ranges above are across runs.
- Synthetic loads; no disk, no network, no real codec.
- The allocator comparison calls each allocator directly through `GlobalAlloc`, not as
  the global allocator; the filter benchmark uses mimalloc as the global allocator.

Must rerun on **Linux x86-64** (the HITL `ubuntu-test-bot` or a cloud VM with pinning):
- `alloc` (glibc instead of libmalloc; mimalloc and snmalloc rankings may change).
- `handoff` with pinned threads and futex parking; the wake cost (5 to 10 us remote
  idle per the UMCG post) decides the spin window default.
- `latest` and `arc` (x86 SeqCst store is `xchg`; contention costs differ).
- Cross-socket runs on a multi-NUMA VM for pool first-touch effects.

Must rerun on **Raspberry Pi 4 (1 GB)**:
- `arc`, `alloc` cross-thread, `latest`: ARMv8.0 without LSE uses LL/SC loops, so
  contended atomics cost the most here.
- `copy`: tinymembench reports about 2.5 to 2.7 GB/s memcpy on a Pi 4
  ([openbenchmarking](https://openbenchmarking.org/result/2110017-IB-PI4BENCH874)), 25 times
  below the M3; copies weigh more on the Pi.
- `handoff` paced at 1 kHz and 50 kHz: wake costs and power with a spin window vs none.
- RSS at idle with each allocator and the pool reserved but not committed (P1: idle
  under 50 MB).
- Also a Pi 5 with its default 16 KiB-page kernel: confirm mimalloc and snmalloc start
  and jemalloc built for 4 KiB pages fails.

---

## 8. Shapes to decide now vs internals to tune later (item 7)

| Choice | Shape or tunable | Why |
|---|---|---|
| Frame references an interned key set of node-local slots (S1 revision) | **Shape** | Changes `types::Frame`, adds `channel::Slot` and `KeySet`, and gives `hub` or `home` a slot table |
| Readers receive views (frame + mask); routing by key set | **Shape** | Changes the `hub` reader API (R8 1.11) and `delivery`'s data structures |
| Series is a slice of a shared block; one block per frame; refcount per frame (S2 revision) | **Shape** | Changes `Series` and `Block`, and every codec and connector writes into it |
| Pools per shard, injected, release returns to the owner; no global pool | **Shape** | Defines the `block` crate (SRP pass) and how `node` wires it; rules out a static pool |
| Offsets, never pointers, inside a block | **Shape** | Keeps a shared-memory SDK path open without a format change |
| Credits counted in bytes beyond the cumulative ack (B3) | Shape of the wire, but wire internals are tuned (RESCOPE) | Settle with the wire section; recommendation stands |
| Global allocator (mimalloc) | Tunable | One line in `node` |
| Size classes, block sizes, pool budget, commit and purge policy | Tunable | Settings and benchmarks |
| Queue kinds (rtrb, own SPSC, Seastar-style pairs) and capacities | Tunable | Behind `block` and shard internals |
| Spin window, batch size, linger | Tunable | Settings per node and link; T1 sweeps |
| Latest mailbox mechanics (atomic swap, triple buffer) | Tunable | Behind `delivery` |
| Biased refcounts, cache-line padding, prefetch | Tunable | Micro-optimizations behind `block` |
| io_uring fixed buffers, O_DIRECT, zero-copy receive | Tunable, Linux-only | Possible later because blocks are aligned and per shard |
| Shared-memory SDK transport | Later feature | Enabled by the offsets rule above |

---

## 9. Decisions for the user (item 8)

Only the shapes need the interview. The tunables take the stated defaults and go to T1.

**M1. Frame keys (item 5).** Should an in-memory frame point to an interned key set
(sorted node-local `u32` slots, one per writer session) instead of owning a
`Vec<channel::Key>` of UUIDs? Recommendation: yes. It follows S1's principle, removes
all UUID hashing from the per-frame path (1.2 to 4 times cheaper per filter, measured),
and makes per-key-set caching possible. `channel::Key` stays the identity everywhere
else.

**M2. Filtered frames (item 5).** Should readers receive a view (frame plus mask), with
masks computed once per (key set, reader) and the home routing each frame by key set?
Recommendation: yes. 4 to 5 ns per frame at any size vs 8 ns to 15 us for a fresh mask
and up to 78 us for rebuilding (measured); it fixes the broadcast-then-filter cost that
Synnax still has (PR #2861).

**M3. Series memory (items 1 and 2).** Should a series be a slice (offset, length) of
one refcounted pool block that holds the whole frame, so a frame costs one refcount and
no allocation? Recommendation: yes. Per-series refcounts cost 18 to 26 times more in
fan-out of a 64-series frame to 4 to 8 readers (measured), and a block per series wastes
most of a block on small series.

**M4. Pool ownership (items 1 and 2).** Should each shard own its pool (injected by
`node`, no global), with blocks returned to the owner when another shard releases them?
Recommendation: yes. Cross-thread frees cost 6.2 to 7.2 ns through an owner return ring
vs 60 to 130 ns through malloc (4 KiB), and a shared lock-free pool costs 75 to 1,306 ns
per operation under 2 to 8 contending threads (measured). Seastar, sharded-slab,
mimalloc v3, and snmalloc all return frees to the owner.

**M5. Shared memory with local SDKs (item 2).** Should block contents use offsets only,
never pointers, so a future same-host transport can map blocks into SDK processes?
Recommendation: yes for the layout rule now; build the shared-memory transport later,
after the local socket path, because SDK-to-node data must be copied for safety unless
memfd seals are used (Linux only).

**M6. Synchronization defaults (items 3 and 4), not an interview item.** Proposed
defaults for T1 to confirm: SPSC rings between shard pairs carrying 8-byte handles,
batched; spin-then-park with a per-node spin window (0 on a Pi); depth-1 swap mailbox
for latest readers; byte credits from the link's BDP; every queue reports depth and
time in queue as status channels. The user only needs to hear that these are settings,
not shapes.

**M7. Global allocator (item 1), not an interview item.** mimalloc 3 as
`#[global_allocator]`, off the hot path only; rule out jemalloc for the ARM64 build
because of fixed page size; re-measure on Linux and the Pi.
