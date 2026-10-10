- **SHARD POOLS (2026-10-06)** `Node::start` makes one `block::Pool` for each shard
  and moves it into the shard, which drops it (M4). Each of `n` shards gets
  `budget / n`, and shard 0 also gets the remainder, so the parts add up to the node's
  budget (MEMORY BOUNDS). The memory comes from `node::Config::memory`, a closure that
  `node` calls once per shard, in order of core: production passes
  `os::memory::Memory::new`, and `sim` tests pass `block::Heap`. A shard with no memory
  is a start failure: later shards do not start, the node stops, and `join` gives
  `Error::Memory` with the core and the `os::memory::Error`. Lost: making the pool on
  the shard's thread, which needs a second path for the error and a `Send + Sync`
  seam. The purge timer and `reclaim` on each loop turn land with the first PR that
  allocates from a pool, since no test can see either before then (#410). Proposed
  by `ops` in #410; approved by the coordinator on #806. `block::Config::new` takes
  the budget as a `u64` and gives `block::Unfit` when the pool's reservation is more
  than `usize::MAX` bytes, so `node` passes each part as it is and keeps no check of
  its own. Decided by `laptop.architect` (2026-10-07 10:27 UTC):
  https://github.com/synnaxlabs/foundation/issues/1317#issuecomment-6036013562, and
  made `const fn` by `laptop.architect` (2026-10-10 06:37 UTC):
  https://github.com/synnaxlabs/foundation/issues/1317#issuecomment-6094721967.
