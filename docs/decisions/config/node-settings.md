- **NODE SETTINGS (2026-10-05)** A node's disk budget and pool budget are a policy that
  selects node names: `node_settings "<name>" { select, disk, pool }`, such as `select =
  "site_a.*"` and `disk = "200GiB"`. Each budget is optional and above zero. A node that
  no policy selects computes a default from its free disk and memory at start, so a mesh
  with no policy works. Before it reads the spec, a node uses the last budget it
  applied, which it keeps in its data directory; the first start uses the default. A
  policy that sets no budget is a user mistake, refused as normal validation with a fix
  (DIAGNOSTICS, #869, #1000). The data directory is node-local: a start argument of
  `foundation`, with a default, because the spec is stored in it. Node-local config for
  the budgets lost: `plan` cannot show it and `apply` cannot change it. Proposed by
  `ops`; the person decided on 2026-10-05 ("Yeah mesh node"), #342. The `config` builder
  added the label and the bound above zero (#474). Each shard's part of the pool budget
  must hold the largest block its buffer takes; a smaller part stops the node at start
  with `Error::Buffer`. `config` cannot check it, because the shard count belongs to the
  node, so the buffer is the one place that refuses it. Decided by the architect on
  #1062: https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343. The
  default is a quarter of the available memory, up to 1 GiB, and a quarter of the free
  disk of the data directory, up to 8 GiB. Decided by `laptop.architect-2` on #1732
  (2026-10-09T18:30:45Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545). On
  Linux the available memory counts the memory cgroup: it is the lesser of
  `MemAvailable` and the room left in the memory cgroup of the process and in each
  cgroup above it (`os::memory::available`). The room is the limit less the working set,
  which is the usage less the inactive file pages (`inactive_file`, or
  `total_inactive_file` on cgroup v1, in `memory.stat`), as the kubelet counts it. A
  cgroup with no `memory.stat`, as under gVisor, counts its whole usage as the working
  set. Each subtraction saturates, because the files are read one after another. Lost:
  `active_file` too. Decided by `laptop.architect-2` (2026-10-09T22:37:18Z,
  https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6090424579, which
  supersedes item 2, the cgroup doc, of
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6088506863;
  2026-10-09T23:43:24Z, no `memory.stat`,
  https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6091170677;
  2026-10-09T23:57:20Z, that sentence here,
  https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6091307461). `os`
  parses `/proc/self/mountinfo` and `/proc/self/cgroup` from their bytes. Lost:
  `procfs-core`, whose parsers take only `&str`, so a path that is not UTF-8 failed the
  read. Decided by `laptop.architect-2` (2026-10-10T03:13:38Z,
  https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6093191608). The node
  keeps its budgets in the file `budget` of its data directory: one sector with the tag
  `foundation/budget/1`, the two budgets as `u64`, and a CRC32C. `node::budget` reads it
  outside the lock. Under the lock, once each shard has opened its buffer, shard 0
  writes it when it is not there, so a default that gives a shard too little is never
  kept. A file that is there stays as it is, also when it holds other budgets. A file
  that a node did not write stops the start with `Error::Budget`. Later work: the first
  `node_settings` policy that a node applies writes over the file `budget`. Decided by
  `laptop.architect-2` on #1732 (2026-10-09T20:14:29Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6088506863;
  2026-10-09T21:08:23Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6089288397). A
  budget that gives a shard too little stops the start with `node.disk` or
  `node.memory`, whose message names where the budget came from (the file `budget`, the
  most that a first start gives, or a quarter of the free resource) and whose fix fits
  it. At the most, the fix is fewer cores, such as a smaller CPU affinity set. Memory
  that the system refused is `node.memory` too, with its own message, "the system
  refused memory for shard-{core} that its part of the pool budget {budget} has room
  for: {error}", and the fix "Free memory on this host". NODE START gives the code of
  each other buffer error. Lost: one text for each budget, also at the most, which tells
  the operator to free memory that does not raise the budget; and `Refused` as
  `node.failed`, which gives no fix for a cause that has one. Decided by
  `laptop.architect-2` on #1732 (2026-10-09T21:35:43Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6089651601;
  2026-10-09T22:37:18Z, the text of `Refused`, which names the part of the shard and
  supersedes item 2, the text of `Refused`, of
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6089651601,
  https://github.com/synnaxlabs/foundation/pull/2191#issuecomment-6090424579). A part of
  the pool budget whose reservation is more than `usize::MAX` bytes stops the start with
  `node::Error::Pool`. From the file `budget` it is `node.memory`, with the message "the
  pool budget {pool}, which {data} keeps from its first start, gives one of {cores}
  shards a pool that needs more address space than this host has" and the fix of a kept
  budget. A first start gives at most 1 GiB, so from another origin it is `node.failed`
  with its text. A kept pool budget that the system cannot reserve
  (`os::memory::Error::Reserve`) is `node.memory` too: "the pool budget {budget}, which
  {data} keeps from its first start, needs more address space for shard-{core} than the
  system gives: {error}", with the same fix. Each other `Error::Memory` is
  `node.failed`. Lost: `node.failed` for each case but the first, which leaves a kept 1
  PiB budget with no fix that names the file; and a fix for a budget too large at each
  origin, for cases that cannot happen. Decided by `laptop.architect-2`
  (2026-10-10T16:07:26Z,
  https://github.com/synnaxlabs/foundation/pull/2288#issuecomment-6099493909).
