- **MEMORY BOUNDS** A hard pool budget per node. Pools reserve address space, commit
  pages lazily, and purge after idle. Credits cap the blocks a reader can pin, apart
  from the frames of the last release of each index that wait for a grant (CREDIT
  RULES). A reader that falls behind is served from disk. When the pool is full, a live
  write records a gap and backfill waits. The current value of B4 pins one block per
  index that had a live frame, with no reader open and no cap. A smaller copy is a
  tunable of `docs/decisions/open/parameters.md`. The person decided on 2026-10-05:
  "Accept it" (#139). The budget counts what stays resident. A purge of a block smaller
  than a page gives no page back, so it frees no budget. When an allocation finds no
  room, the pool gives back the whole carved range and budget of size classes whose
  carved blocks are all free, until the allocation fits. Under this pressure, at most
  two partial pages per class stay resident, and `Config::budget` states that slack. An
  idle class that no allocation presses keeps its pages until the purge after idle. A
  class that a reader keeps partly in use keeps its budget. The person accepted this
  (design H) on 2026-10-05 ("Ok fine"), #2, #270. Purges per block that give back every
  page they credit (design P) wait in a follow-up issue. When the system refuses to
  commit pages, the pool gives back one idle size at a time, in the order a purge for
  room in the budget uses, and tries the commit again; after the last idle size the
  allocation fails with `Error::Refused`, a separate error from a full pool (the person
  on 2026-10-05: "I approve the separate error"). The carve counts do not change, the
  sizes given back stay given back, and a later allocation may succeed (#475, #542).
