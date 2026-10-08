- **SPEC APPLY (#1083)** `Mesh::apply(base, definitions, homes)` makes the definitions,
  by tree key, the region's spec through the leader, as `set_home` does, and gives the
  new pointer. It first runs `spec::region::check` (REGION CHECK) at the region's
  prefix: a problem gives `Error::Problems`, which holds each problem as `check` gives
  it, and proposes nothing. `mesh` defines no problem of its own. `homes` maps index
  names to node names (S12 (placement part) + B7). An index that `definitions` does not
  hold as an index channel gives `Error::NotIndex`, then more than `HOMES_MAX` homes
  give `Error::Homes`, both before the read of the base tree. `Homes` counts only the
  listed indexes with no home in this node's state then. `UnknownNode` looks up only the
  nodes of the listed indexes with no home when the call checks the node names, which
  are among those, and the change takes only those indexes (`laptop.architect`,
  2026-10-08T18:35:16Z:
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633). The two
  reads decided by `laptop.architect`, 2026-10-08T21:37:03Z
  (https://github.com/synnaxlabs/foundation/pull/2007#issuecomment-6069527551), which
  changes the one read in `Homes` of 6066553633 and the SPEC APPLY text of
  https://github.com/synnaxlabs/foundation/pull/2007#issuecomment-6069454501. A node
  name that no member has gives `Error::UnknownNode`, after `Error::NoVote` and before
  the first put. None of them proposes anything. `Homes` right after `NotIndex` decided
  by `laptop.architect`, 2026-10-08T17:01:08Z
  (https://github.com/synnaxlabs/foundation/issues/1154#issuecomment-6064957210), and
  the order as a whole approved at 2026-10-08T17:15:26Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065202125). It then
  builds the tree with `spec::region::tree`. The change lists each chunk of the new tree
  that the tree of the base lacks, or each chunk of the new tree when `Config::store`,
  the node's `blob::Store`, cannot give the tree of the base. A change that lists more
  than `CHUNKS_MAX` chunks gives `Error::Large { chunks, most }` and proposes nothing. A
  base root whose chunk is not a tree node is a base tree that the store cannot give.
  The node counts itself as the one holder. When the holders are not a majority of each
  half of the voters, before the propose or at the apply, the call gives `Error::Quorum
  { held, voters }` for the first half that lacks one, the incoming half first. The
  count before the propose costs no entry and no put. The node then puts each chunk of
  the new tree in its store, not only the listed ones, because `diff` never reads a
  chunk that the two trees share, so a chunk that the store lost is found only by a put.
  On `Ok`, a put of each chunk of the new tree has returned. BLOB STORE gives what a put
  holds after a fault. Decided by `laptop.architect`, 2026-10-08T12:13:51Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059603551), which
  supersedes the postcondition of item 2 of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643, and the
  second sentence 2026-10-08T12:23:54Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059781924), which
  supersedes the second sentence of the SPEC APPLY text of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059603551. A put
  gives `Error::Pool` or `Error::Blob` on a failure. `Mesh::open` puts each chunk of the
  founding tree in the store. A change that the state refuses as stale gives
  `Error::Stale { base, pointer }`. A call learns the refusal of its own entry from
  `Applied`, which keeps the refusal of each applied entry above the lowest open floor
  of a try. Decided by `laptop.architect`, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836). A call
  whose entry finds the pointer that the call makes, after a lost answer or an equal
  change of another call, returns that pointer, under the home rule below; a later
  pointer gives `Stale`. Decided by `laptop.architect`, 2026-10-08T10:19:54Z
  (https://github.com/synnaxlabs/foundation/pull/1855#issuecomment-6057736427). The
  equal change of another call, 2026-10-08T11:46:44Z
  (https://github.com/synnaxlabs/foundation/pull/1855#issuecomment-6059166107). A listed
  home is a proposal: the entry gives it only to an index with none. On `Ok`, the
  pointer is the call's and each listed index has a home in this node's state when the
  call settles, the listed one or another. A call whose entry finds `base.next(root)`
  returns it when each listed index has a home then, and else gives `Stale`. No entry
  removes a home, so the path on which the call's entry applies needs no check, and both
  paths give the same result. Trigger: when an entry can remove a home, that path checks
  too, and so does the count of `Homes` at the name check (that count,
  2026-10-08T21:37:03Z:
  https://github.com/synnaxlabs/foundation/pull/2007#issuecomment-6069527551). Decided
  by `laptop.architect`, 2026-10-08T17:20:54Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065295958), and
  changed by `laptop.architect` at 2026-10-08T17:34:20Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065525915), which
  supersedes item 2 of
  https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065295958. Those two
  rulings supersede, in
  https://github.com/synnaxlabs/foundation/pull/1855#issuecomment-6059166107, the `Ok`
  of an equal change of another call that leaves a listed index with no home. The
  `Stale` item of `Mesh::apply` names that case, approved by `laptop.architect` at
  2026-10-08T17:52:34Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065835401). The
  reading when the call settles, the invariant, and its trigger, decided by
  `laptop.architect` at 2026-10-08T18:00:15Z
  (https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065963448), which
  supersedes the `Stale` text of
  https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065835401 and the
  `Ok` text of
  https://github.com/synnaxlabs/foundation/pull/1934#issuecomment-6065525915. The build
  with `spec::region::tree`, and the build of the root of `Config::founding.definitions`
  with it in `Mesh::open`, decided by `laptop.architect`, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151).
  Supersedes the build with `spec::tree::apply` from `tree::empty()`
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771). The
  listed chunks, the store, the put, the founding put, and `Quorum`, decided by
  `laptop.architect`, 2026-10-08T11:03:36Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178).
  Supersedes the list of each chunk of the new tree and `Large` on its count
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836). The
  put of each chunk of the new tree, the count before the put, the full list on a base
  root that is not a tree node, and the name `Config::store`, 2026-10-08T11:53:51Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643).
