- **NODE SETTINGS (2026-10-05)** A node's disk budget and pool budget are a policy
  that selects node names: `node_settings "<name>" { select, disk, pool }`, such as
  `select = "site_a.*"` and `disk = "200GiB"`. Each budget is optional and above zero.
  A node that no policy selects computes a default from its free disk and memory at
  start, so a mesh with no policy works. Before it reads the spec, a node uses the last
  budget it applied, which it keeps in its data directory; the first start uses the
  default. A policy that sets no budget is a user mistake, refused as normal
  validation with a fix (DIAGNOSTICS, #869, #1000). The data directory is node-local:
  a start argument of `foundation`, with a default, because the spec is stored in it.
  Node-local config for the budgets lost: `plan` cannot show it and `apply` cannot
  change it. Proposed by `ops`; the person decided on 2026-10-05 ("Yeah mesh node"),
  #342. The `config` builder added the label and the bound above zero (#474).
  Each shard's part of the pool budget must hold the largest block its buffer takes;
  a smaller part stops the node at start with `Error::Buffer`. `config` cannot check
  it, because the shard count belongs to the node, so the buffer is the one place
  that refuses it. Decided by the architect on #1062:
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343.
  The default is a quarter of the available memory, up to 1 GiB, and a quarter of
  the free disk of the data directory, up to 8 GiB. Decided by `laptop.architect-2`
  on #1732 (2026-10-09T18:30:45Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545).
  On Linux the available memory counts the memory cgroup: it is the lesser of
  `MemAvailable` and the room left in the memory cgroup of the process and in each
  cgroup above it (`os::memory::available`). The node keeps its budgets in
  the file `budget` of its data directory: one sector with the tag
  `foundation/budget/1`, the two budgets as `u64`, and a CRC32C. `node::budget`
  reads it outside the lock. Under the lock, once each shard has opened its buffer,
  shard 0 writes it when it is not there, so a default that gives a shard too little
  is never kept. A file that is there stays as it is, also when it holds other
  budgets. A file that a node did not write stops the start with `Error::Budget`.
  Later work: the first `node_settings` policy that a node applies writes over the
  file `budget`. Decided by `laptop.architect-2` on #1732 (2026-10-09T20:14:29Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6088506863;
  2026-10-09T21:08:23Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6089288397).
  A budget that gives a shard too little stops the start with `node.disk` or
  `node.memory`, whose message names where the budget came from (the file `budget`,
  the most that a first start gives, or a quarter of the free resource) and whose fix
  fits it. At the most, the fix is fewer cores, such as a smaller CPU affinity set.
  Memory that the system refused is `node.memory` too, with its own message and the
  fix "Free memory on this host". Each other buffer error is `node.failed`. Lost: one
  text for each budget, also at the most, which tells the operator to free memory
  that does not raise the budget; and `Refused` as `node.failed`, which gives no fix
  for a cause that has one. Decided by `laptop.architect-2` on #1732
  (2026-10-09T21:35:43Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6089651601).
