- **SHARD DISK (2026-10-07)** Until segments exist, `node` gives each shard's ring
  `disk / n` of the node's disk budget (`node::Config::disk`, a `types::byte::Size`),
  and shard 0 also gets the remainder, as SHARD POOLS does. The budget bounds each new
  ring file: `node` takes the largest ring whose file fits each part
  (`buffer::Layout::fit`), so the format stays in `buffer`. When a part holds no ring,
  no shard starts, and `join` gives `Error::Disk` with the budget, the shard count, and
  the least budget (`n` times the least ring that `fit` gives, capped at the largest
  `Size`); `config` cannot check it, as for the pool part (NODE SETTINGS). The ring is
  the whole store. A ring with a checkpoint keeps its size, which can be more than its
  part, until `Buffer::resize` exists (#451). A ring with none is made again at its part
  (#1254). So oldest first (B1) holds per shard, not per node. This is a patch. The
  long-term path is small rings for commits, then segments that draw from one node-wide
  allowance (#1081). The lab of `docs/decisions/open/mvp.md` sizes the budget for the
  shard that holds the index. With `Buffer::resize`, `node` computes the `Layout` with
  `fit` and calls `Buffer::resize`, nothing more: `buffer` sets the file length itself,
  so `node` never extends or cuts the ring file and does not learn the format (after
  https://github.com/synnaxlabs/foundation/issues/451#issuecomment-6032821843). Decided
  by the architect, #342:
  https://github.com/synnaxlabs/foundation/issues/342#issuecomment-6030837040,
  https://github.com/synnaxlabs/foundation/issues/342#issuecomment-6032845187,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033305257,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033404672,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033884447, and, for a
  ring with no checkpoint (2026-10-07T08:52:34Z),
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6034449677.
