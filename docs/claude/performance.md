# Performance rulebook

Foundation targets (P1): 100M samples/s per node; within 2x at 100k channels;
latest-mode p99 under 250 µs over one encrypted LAN hop; under 4 bytes per sample;
a Raspberry Pi 4 idles under 50 MB and starts in under 1 s. A regression over 5% blocks
a merge.

Evidence: `docs/research/r1-thread-model.md`, `docs/research/r11-memory-sync.md`, and
`docs/research/r11-mem-bench/`. The numbers below are from an M3 Max. Rerun on Linux
x86-64 (pinned cores) and on a Pi 4 before you rely on them.

## Rules

1. **No heap allocation on the hot path.** Frames come from the shard's pool (alloc and
   free 1.1 ns, against 4.2 ns for mimalloc and 9.2 ns for macOS malloc). A counting
   allocator in tests fails any hot-path allocation.
2. **One reference count per frame, never per series.** Fan-out is 18-26x cheaper. One
   shared `Arc` under 8 threads costs about 430 ns: no shared reference-count hot
   spots.
3. **Each shard owns its pool.** A release returns the block to the owner (6-7 ns,
   against 60-130 ns for a malloc free on another thread). No global or shared
   lock-free pools (75-1,306 ns under contention).
4. **No UUIDs on the hot path.** Use node-local slots, interned key sets, and cached
   masks per (key set, reader). Filtering is a view, never a rebuild (4-5 ns, against
   up to 78 µs).
5. **No locks on a shard's hot path.** Traffic between shards goes through
   single-producer, single-consumer rings that carry handles, in batches (49-77 ns;
   0.6 ns batched).
6. **Wakeups are expensive** (5.5-8 µs, with millisecond p99.9 tails under load).
   Batch them. Spin within a per-node window (0 on a Pi). Check every wake protocol
   with loom or shuttle, and fence both sides: spin-then-park hung on Apple silicon
   because an RCpc load (LDAPR) passed the store of the sleep flag.
7. **Measure atomics on ARM without LSE (Pi 4) and on x86.** Pad hot atomics to a cache
   line.
8. **Every queue is bounded**, reports its depth, is sized by Little's law, and is
   tunable.
9. **Blocks hold offsets, never pointers**, so memory shared with SDKs stays possible.
10. **Every copy in the data path names its reason** and a benchmark that shows it
    beats the alternative. Count copies per sample from device to wire to disk.
11. **Per-frame cost scales with interested readers, not with channel count.**
12. **Claims are measured, never inferred.** Name the machine.
13. **Memory is bounded everywhere.** A hard per-node pool budget. Pools commit pages
    lazily and purge after idle time. Credits cap the blocks a reader can pin. A
    reader that falls behind is served from disk, never from pinned memory. A live
    write that finds the pool full records a gap instead of waiting.
14. **Facts known per writer session (types, keys) are interned once** in the key set,
    never repeated per frame or per series.

## The six questions

Every PR that touches a hot path answers these in its description:

1. Where does it allocate?
2. Which atomics and locks does it add?
3. Which thread owns each piece of state, and what crosses a thread?
4. How many copies per sample?
5. Does per-frame cost grow with channel count?
6. What wakes whom, and is it checked with loom or shuttle?
