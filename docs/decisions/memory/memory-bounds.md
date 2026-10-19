- **MEMORY BOUNDS** A hard pool budget per node. Pools reserve address space, commit
  pages lazily, and purge after idle. Credits cap the blocks a reader can pin, apart
  from the frames of the last release of each index that wait for a grant (CREDIT
  RULES). A reader that falls behind is served from disk. When the pool is full, a live
  write records a gap and backfill waits. The current value of B4 pins one block per
  index that had a live frame, with no reader open and no cap. A smaller copy is a
  tunable of `docs/decisions/open/parameters.md`. The person decided on 2026-10-05:
  "Accept it" (#139). The budget counts what stays resident (on macOS, what the pool
  holds: see below). A purge of a block smaller than a page gives no page back, so it
  frees no budget. When an allocation finds no
  room, the pool gives back the whole carved range and budget of size classes whose
  carved blocks are all free, until the allocation fits. Under this pressure, at most
  two partial pages per class stay resident, and `Config::budget` states that slack. An
  idle class that no allocation presses keeps its pages until the purge after idle. A
  class that a reader keeps partly in use kept its budget: the person accepted this
  (design H) on 2026-10-05 ("Ok fine"), #2, #270. Amended (#2810; this supersedes the
  design H sentence before it, #270; `laptop.architect`, 2026-10-16T19:23:50Z:
  https://github.com/synnaxlabs/foundation/pull/2907#issuecomment-6223028676): when the
  sizes with no block in use do not cover an allocation, the pool gives back the payload
  pages of free blocks of the other sizes, and credits the budget by the whole pages of
  each payload. It gives back no page unless all of these together cover the allocation.
  Each such block keeps its header and the partial pages at the ends of its payload,
  with their budget. It gives back its pages with a discard, which keeps their
  protection and their commit charge, so it splits no mapping. An allocation of its
  size takes back its budget before it carves a new block, and makes no commit, so the
  system cannot refuse it. Under strict overcommit (`vm.overcommit_memory=2`), the
  charge of such a block stays until its size goes idle, and a carve past the limit of
  the system then gets `Error::Refused`. On Linux with the default overcommit, its
  pages leave the resident set at once. On macOS they stay resident, and in the
  footprint of the task, until the system needs memory (BLOCK MEMORY), so there the
  budget bounds the memory that the pool holds, not its footprint. `MADV_FREE_REUSABLE`
  drops the footprint at once, but lost: it needs a second `Memory` call before the
  pool writes the block again, and with no such call the footprint shows less than the
  task holds (`laptop.architect-2`, 2026-10-16T19:41:36Z:
  https://github.com/synnaxlabs/foundation/pull/2907#issuecomment-6223324987). A free
  block whose payload holds no whole page keeps its budget, so a waiter of another size
  can wait until the last block of that size frees. Decided by
  `laptop.architect` (2026-10-16T17:05:20Z):
  https://github.com/synnaxlabs/foundation/issues/2810#issuecomment-6220640348; the
  discard by `laptop.architect` (2026-10-16T19:37:29Z), after the objection of
  `laptop.monitor` to a cap on such blocks:
  https://github.com/synnaxlabs/foundation/pull/2907#issuecomment-6223259062. The
  rest of design P, the headers in a table and the pages of blocks with no whole page,
  waits on #291, before the exit of ONE NODE. When the system refuses to
  commit pages, the pool gives back one idle size at a time, in the order a purge for
  room in the budget uses, and tries the commit again; after the last idle size the
  allocation fails with `Error::Refused`, a separate error from a full pool (the person
  on 2026-10-05: "I approve the separate error"). The carve counts do not change, the
  sizes given back stay given back, and a later allocation may succeed (#475, #542).
  Outside the pool budget, each QUIC connection holds on the heap: the sent data that
  noq keeps (STREAM WIRE says how long), at most `window_bytes` (`send_window`) plus,
  for each stream, the copied stretch at its front, at most the peer's
  `message_bytes_max`, which noq frees only once the peer acknowledges each byte of it,
  or the stream resets; the rest of each copied stretch that noq has not taken, at most
  the peer's window (the send budget); the stretch buffer of `send_parts`, under twice
  the peer's `message_bytes_max`; and the parts list of each sending stream, 24 bytes
  for each part of its capacity, at least 4 parts once it keeps one, which grows by
  doubling to the most parts that the stream kept of one message and stays until the
  stream ends (a count that the caller's messages bound, not the peer) (STREAM WIRE;
  `laptop.architect-2`, #1879, 2026-10-08 16:29 UTC:
  https://github.com/synnaxlabs/foundation/pull/1879#issuecomment-6064411384, #2304,
  2026-10-10 22:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/2304#issuecomment-6102910746, and #2304,
  2026-10-10 23:27 UTC:
  https://github.com/synnaxlabs/foundation/pull/2304#issuecomment-6103308877, which
  supersedes the parts-list clause of 6102910746; the copied stretch at the front of
  each stream: `laptop.architect-2`, #2439, 2026-10-12 19:46 UTC:
  https://github.com/synnaxlabs/foundation/pull/2439#issuecomment-6131444028, which
  supersedes the bound of the sent data in 6064411384; its free at a reset:
  `laptop.architect-2`, #2361, 2026-10-11 17:39 UTC:
  https://github.com/synnaxlabs/foundation/issues/2361#issuecomment-6111809297). A
  stream ends at the last of its sends, at a peer stop, at a cancel that resets it, and
  at the close of its connection by either side (the same 6103308877). Limits: the heap
  of the sent data that noq keeps is more than these bytes: each kept copy of a short
  write keeps its whole allocation, and the queue slot of each kept segment, the shared
  header of each copy, and the owner box of each block that noq keeps a run of are not
  counted (#2483; `laptop.architect-2`, #2483, 2026-10-12 19:58 UTC:
  https://github.com/synnaxlabs/foundation/issues/2483#issuecomment-6131627109, and
  #2439, 2026-10-12 20:05 UTC:
  https://github.com/synnaxlabs/foundation/pull/2439#issuecomment-6131721896, which
  supersedes the Limits sentence of 6131627109). Outside the pool budget, each shard's
  `home::stored::Decoder` holds on the heap the entry maps of the last 4 key sets that
  a read from disk filled onto, each with the capacity of the largest set that it held,
  about 49 bytes for each channel of that set, and scratch lists with the capacity of
  the largest entry that it decoded. Nothing frees them before #2761 (#274 B1;
  `laptop.architect`, 2026-10-15T21:30:23Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6202690738, item 1;
  this text: `laptop.architect`, 2026-10-15T21:51:47Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6203034272, item 2,
  which supersedes the text of 6202866145 item 2). Outside the pool budget,
  the request bodies, read opens, write opens, and ends of write streams of a hub
  count in its body budget, `BODIES_BYTES_MAX` (32 MiB), and those of one subject in
  `BODY_BYTES_MAX` (16 MiB) (HUB CLIENT). The ends of a write stream are 16 bytes for
  each channel of its open and one for its index, from the decode of its open until
  the stream ends. Its decoded names, 16 bytes and the length of each, count from that
  decode until the stream sends its places. A write open is at most `OPEN_BYTES_MAX`
  (64 KiB), with at least 2 bytes for each channel, so its ends are at most 512 KiB,
  and the budget bounds the places of all write streams of a node at 2,097,152. The
  cost: the writers of a subject hold their ends in its share until their streams end,
  so wide writers can make the requests and read opens of that subject stop with
  `BUSY`. Limits: the budget counts only the ends and the names. Other memory of a
  write stream that grows with its channels (its key set and the home's writer) is not
  counted (`laptop.architect`,
  2026-10-18T07:45:01Z:
  https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6247275705, item
  5, which closes the deferral of
  https://github.com/synnaxlabs/foundation/pull/3148#issuecomment-6246759669, item 3).
