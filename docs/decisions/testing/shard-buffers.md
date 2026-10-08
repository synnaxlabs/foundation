- **SHARD BUFFERS (2026-10-07)** Each shard opens its write-ahead ring in directory
  `shard-<i>` of the node's data directory and keeps it until the node stops.
  `node::Config::files` is a maker with no core that `node` calls on the start
  thread, in order of core, for each shard that gets its memory, just before its
  start; the shard runs the function it gives on its own thread, because `Files` is
  `Rc`, and a shard that does not start drops it unrun. A caller on the real OS makes
  each shard's disk with `os::files` before the start and joins its I/O thread after
  `join`. A `Fn` that each shard calls on its own thread lost: it fits `os::files` only
  with a lock around a queue of disks. A fallible maker like `memory`, with a `node`
  error for it, lost: `node` would then own I/O thread handles, which `sim` does not
  have. Decided by the architect on #1062 (#1173):
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6032037030.
  `node` alone names `shard-<i>`. `node::Config::entropy` gives the shards
  randomness. A ring that does not open stops the node, and `join` gives
  `Error::Buffer` with the core, after `Start` and `Memory` and before `Panicked`. A
  data directory made for another shard count, more or fewer, is refused before any
  buffer opens (#1076); a reshard at start is the long-term path (#1077). Running the
  stored count on another core count lost: it bends C2. Decided by the architect on
  #1062:
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343. The
  count is an empty directory `shards-<n>` in the data directory, made and synced
  before `shard-0`, so a crash leaves it whole or absent. Shard 0 claims it at the
  head of the interner handoff. Another count gives `Error::Shards`, and a failed
  file call `Error::Directory`. Any record of another count fails the start, also
  next to `shards-<cores>`, and `stored` is the smallest such count, so the error
  does not hang on the order of the list. A name counts only when the rest after
  `shard-` or `shards-` is plain decimal that fits a `usize`: above zero for a
  record, and below `usize::MAX` for a ring. Any other name (`shards-03`,
  `shards-+3`, `shards-0`, `shard-<usize::MAX>`) is one the claim does not know, and
  it ignores it, because no core count makes the node write it. A count that no host
  has, such as `shards-<usize::MAX>`, is still a record, and the claim refuses the
  start (decided by `laptop.architect-2`, 2026-10-07T08:16:03Z, #1214:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6033877711, with
  "node" for "claim" at 2026-10-07T16:47:47Z:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6042568803).
  Supersedes the reason in
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6032550887. The
  claim reads names only, so a file with such a name counts as a directory would.
  Decided by the architect, #1214:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6032550887, as on
  #1110 for `shards-0` and `shard-<usize::MAX>`:
  https://github.com/synnaxlabs/foundation/pull/1110#issuecomment-6032339719. With
  no record, rings up to `shard-<k>` are a record of `k + 1`, so a data directory
  whose record a copy dropped is checked too; a crash cannot leave a ring with no
  record. Each start syncs the data directory before `shard-0`, also when the record
  is there, because a process crash can leave it unsynced. A one-sector file lost: it
  needs a block, a write, two syncs, and a decode. Decided by the architect, #1076:
  https://github.com/synnaxlabs/foundation/issues/1076#issuecomment-6031257049.
  The rule of rings with no record stays, decided by the architect on #1178:
  https://github.com/synnaxlabs/foundation/issues/1178#issuecomment-6032340796, with
  the reasons at
  https://github.com/synnaxlabs/foundation/pull/1110#issuecomment-6032339719. After a
  stop, a shard starts no disk step: shard 0 checks the stop before the claim, and
  each shard before its open. A started step runs to its end. A skipped step drops its
  handoff, so each later shard skips too. A stop is not a failure, so `join` gives
  `Ok` when no shard failed. Any failure stops the node, so a claim or open that has
  not started does not start; `join` gives `Start` or `Memory`, else `Shards` or
  `Directory`, else `Buffer` by core, else `Panicked` by core. Decided by the
  architect on #1062 (#1174):
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6032037030. One
  shard writes each name in the data directory: shard `i` writes `shard-<i>` and each
  name in it, and shard 0 also writes `lock`, `shards-<n>`, and, with a region, `mesh`
  and each name in it (#585, by `laptop.architect`, 2026-10-08 03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475), and
  `blob` and each name in it (#1741, approved by `laptop.architect-2`, 2026-10-08
  11:50 UTC:
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059221889). A
  change that gives a name a second writer first changes the check of FILE RENAME,
  which relies on this (#1503, decided by `laptop.architect-2`, 2026-10-07 19:12 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044987221).
