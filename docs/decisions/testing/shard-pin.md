- **SHARD PIN (#718, 2026-10-05)** `Shards::pinnable()` says whether a shard can pin
  to a core: `true` on Linux, `false` on other OSes, and `true` in `sim` unless the
  node config says `unpinnable`. `node` sets no core when it is `false`, and logs that
  once at start. `Shards::start` panics on a core then, as on a core past the count:
  the answer never changes, so a core there is a bug in `node`. `Error::Pin` means
  only a real fault, such as a CPU that went offline after the read, and carries the
  cause as a `reason`. This is the advisor's choice A, narrowed from the set of cores
  that can pin to a bool: the index map of ENV SEAMS makes that set always
  `0..cores()` or empty. Lost: `Error::Pin` for a core on a node that cannot pin, which
  gives two contracts for the same kind of bug, and a caller tells the bug from a
  fault only by its `reason` text; each driver checks the core itself, which puts one
  rule in each driver. Windows pinning waits for the person (#477). Amends ENV SEAMS.
