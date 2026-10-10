- **COUNTING ALLOCATOR (2026-10-04)** The person allowed one exception to "no mutable
  globals": "Allow in test binaries". A test or benchmark binary may hold one
  counting `#[global_allocator]` `static` with an atomic count, because Rust has no
  other way to count allocations. Never in a library or the `node` binary. The
  `xtask globals` check allows only this case. The static also holds the state of
  `Allocator::freed_holding` (#349): a phase with a count of the frees that scan, the
  caller's needle while a call runs, and a found count, because Rust has no other way
  to see a freed block. `freed_holding` is the one exception to "Safe code is sound
  for every input": the person said "#481 I approve A" on 2026-10-05. A freed block
  can hold bytes the program never wrote, such as padding or the spare capacity of a
  `Vec`. Rust defines no read of such a byte on any target, so no sound read exists,
  and Miri stops at one. This is a patch. The long-term fix is a freeze read (Rust RFC
  3605); when Rust has one, `freed_holding` uses it and the exception goes. A binary
  that bounds the memory of a structure holds `counting::Bytes`, an atomic count of the
  bytes it holds and of the most it held; a binary holds one counting allocator,
  `Allocator` or `Bytes`. `Allocator` does not keep that count: a benchmark must not pay
  for a count that only a test reads, or its baseline moves with no product change, as
  `transport/benches/send.rs` did (+4.2% to +11.2% at p50). Lost: `Allocator` keeps
  `held` (that cost in each counting binary); a `bool` at construction (a branch on each
  allocation and free, and a `held` that must panic when it is false);
  `Allocator<const HELD: bool>` (no branch, but `Allocator<true>` says nothing at the
  call site, and no caller needs both counts in one binary). Decided by
  `laptop.architect` on 2026-10-07T16:28:07Z
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6042194242).
  Supersedes the `held` part of
  https://github.com/synnaxlabs/foundation/issues/1437#issuecomment-6040199190.
  A counting allocator runs code as `System` runs it, apart from its count: each
  `GlobalAlloc` method calls the `System` method of the same name, and keeps the
  trait's own body only when the allocator's contract needs it, with a comment that
  names that contract. So `Bytes::realloc` calls `System.realloc` and changes `held` by
  the difference of the two sizes in one atomic step, and `Allocator::realloc` keeps
  the trait's own body, because the scan of a free reads the old block. A count that
  no test can tell apart is not a reason to keep the trait's own body: under it, a
  `realloc` that doubles 64 B to 1 MiB took 15.4 µs, not 1.6 µs (Apple M3 Max).
  Decided by `laptop.architect` on 2026-10-07T17:59:58Z
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6043758919, #1536).
  Supersedes the reason of 5d23e00e
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6042860057).
  `Bytes` keeps the bytes it holds and the most it held in a window: `peak` reads the
  most, and `reset_peak` starts a new window at the bytes held then, so a test can
  bound the memory that one step takes. A read stays a read, as `held` is. A lock
  orders each growth, reset, and read of the peak, so `peak` is never under a count
  held in the window, and an allocation on another thread during the reset counts in
  the new window. Frees and shrinks do not take the lock. No benchmark holds `Bytes`,
  and `Allocator` does not change; a benchmark that holds it later states the cost of
  the lock in its PR. Lost: one `peak` that also resets (a second read gives a value
  that the first changed); `take_peak` (it discards a value to start a window); two
  atomics with no lock (a free between the growth of the count and of the peak lets
  `peak` give less than a count held); one 128-bit atomic (Rust has no stable
  `AtomicU128` on each target). Decided by `laptop.architect` on 2026-10-10T02:42:17Z
  (https://github.com/synnaxlabs/foundation/issues/2116#issuecomment-6092914109), and
  the lock on 2026-10-10T04:06:13Z
  (https://github.com/synnaxlabs/foundation/pull/2230#issuecomment-6093592767), which
  supersedes its swap of the peak to 0. Supersedes "one atomic count of the bytes it
  holds" of https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6042194242.
