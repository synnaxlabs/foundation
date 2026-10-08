- **BLOCK MEMORY (2026-10-04)** A `block::Pool` gets its address space through
  `block::Memory`, a small `unsafe` trait in `block`, because `block` sits below
  `env`. `os` implements it over `mmap` (reserve, commit, purge); `block::Heap`
  implements it over `std::alloc` for tests, Miri, and `sim`. `block` makes no OS
  call. `reclaim` takes back returned blocks on each loop turn; `purge` gives idle
  pages back on a timer that the shard owns (#2). The first 64 bytes of a `Memory`
  are usable from the start: they hold the pool's header, so `Pool::new` makes no
  commit that can fail. A purged page stops counting against the memory the system
  can commit. On Linux with strict overcommit, `madvise` and `mprotect` keep that
  charge, so `os` purges with a `MAP_FIXED` remap (#475). `os::memory::Memory`
  reserves `PROT_NONE` pages, which take no charge, and commits with `mprotect`;
  `ENOMEM` gives `Refused`, and a refused commit can leave part of its range
  committed and charged until a purge or the drop. A failed purge remap panics, and
  the drop then leaks the reserve: on Linux the remap can leave a hole that another
  mapping fills, and an unmap would remove that mapping. `os::memory` builds on Linux
  and macOS only; Windows waits for #477, and `node` adds no cfg for it. On Linux each
  reserved or purged page has no huge pages (`MADV_NOHUGEPAGE`): the first touch of
  a huge page takes 2 MiB, and a purge of part of one gives memory back only later.
  A read and write `MAP_NORESERVE` reserve with a commit that does nothing lost: strict
  overcommit and Windows charge it in full, and it never refuses (#66). The person
  approved `unsafe` in `os::memory`, checked by tests on the real OS and not by Miri, on
  2026-10-05 ("Yeah taht's fine"), #461. `block::testing::{Scarce, Switch}`, behind
  the `sim` feature, is heap memory whose commits a test makes refuse, so a crate
  above `block` tests a refused commit through its production path (#591).
  `Pool::heap(config)` makes a pool on a `Heap` of `Config::reservation` bytes, so a
  caller that wants heap memory does not size it. `Pool::new` stays for injected
  memory, such as `os::memory::Memory` in `node` and `testing::Scarce` in a test
  (decided by the architect, 2026-10-07T08:55:57Z, #1294:
  https://github.com/synnaxlabs/foundation/issues/1294#issuecomment-6034509040).
