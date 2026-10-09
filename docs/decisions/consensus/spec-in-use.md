- **SPEC IN USE (#1741)** `Mesh::spec` gives the node's `used::Spec`: the pointer and
  the definitions of the spec in use, and `Behind` with the newest committed pointer
  whose read ended, when the node does not use it, with its `Cause`. A newer pointer can
  wait for its read, and a retry keeps the cause of the read before it. A call waits for
  the first read of the pointer committed at the call, or of a later pointer that
  replaced it before its read began, and never for a retry. It gives `Stopped` when the
  group stopped, before or during the call (`laptop.architect`, 2026-10-09T22:22:43Z,
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090235986). One task
  in `mesh` reads the spec of each committed pointer with `spec::region::definitions`
  over the chunks of the spec in use and the chunks that the change lists, and gets each
  chunk that the read gives as `Missing` from `Config::store`. A spec that reads and has
  no problem at `spec::region::check` takes effect. A spec with problems or a tree that
  does not read leaves the last spec in use (SPEC CHANGE). After a missing chunk, a
  retry each second (`RETRY`, on the `env` clock) gets only that chunk, and reads again
  only when the store gives it. A failed store call or file call is read again each
  second. The state is at most two trees: the chunks of the spec in use, from
  `spec::region::tree` of its definitions, and the chunks of the tree of the newest
  committed pointer that the task got. A newer pointer drops the chunks of the one it
  replaces. The base tree of `Mesh::apply` comes from `Config::store` only, never from
  the chunks of the spec in use. The pointer in use is one empty file `<version>-<root>`
  in `<Config::dir>/spec`. A spec takes effect after the create and the sync of the
  directory of its file. The node then removes each other file in the directory, as
  `Mesh::open` does, which keeps only the highest version. A failed removal is not a
  cause: the next change or open removes the file. At an open, the task skips each
  replayed pointer at or below the one that it knows. This is correct while the replay
  passes through the pointer of the file, and Raft never drops a committed entry.
  Trigger: before a forced takeover (K5) can drop a committed entry, its issue decides
  how the task meets the new history. `Mesh::open` reads the spec of the highest version
  that a file names from the store, or uses the founding spec when there is no file. A
  spec that does not read or has problems is not an error of the open: `behind` gives
  the cause. A file that does not name a pointer gives `Error::Stray`, and a failed file
  call `Error::Files`. The restart read gets one missing chunk for each run of
  `definitions`, so it costs time squared in the chunks of the tree. Trigger: before a
  milestone opens a node on a spec of more than `CHUNKS_MAX` chunks, `spec::tree` gets
  one level of a tree at a time (#1231). Decided by `laptop.architect`,
  2026-10-08T11:03:36Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178); the
  read with `definitions`, `Cause::Read`, and the two trees, 2026-10-08T11:13:40Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058621907), with
  `definitions` approved by `laptop.architect-2`, 2026-10-08T11:11:54Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058593807). A
  committed subject at `@admin.@subject` gives `Cause::Problems` and stays out of the
  spec in use, so it never reaches `access::Rules`, 2026-10-08T13:00:02Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6060420592). The
  retry, `Error::Files`, `Error::Stray`, `Cause::Files`, the removal, the doc of
  `Spec::pointer`, the replay skip, and its trigger, 2026-10-08T13:50:20Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6061345871). The
  base tree from the store only, and `Behind` as the newest pointer whose read ended,
  2026-10-08T14:48:55Z
  (https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6062486179). The
  retry clause and the wait of `Mesh::spec`, 2026-10-08T15:01:22Z
  (https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6062732865). The doc
  of `Mesh::open`, 2026-10-08T15:42:09Z
  (https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6063561498).
  Supersedes: the base tree from the spec in use in item 2 of
  https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178, the wait
  of `Mesh::spec` in item 4 of the same comment, and the text of `Behind::pointer` in
  https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6061345871.
