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
  chunk that the read gives as `Missing` from `Config::store`, or else from the peers. A
  spec that reads and has no problem at `spec::region::check` takes effect. A spec with
  problems or a tree that does not read leaves the last spec in use (SPEC CHANGE). After
  a read that misses chunks, the task gets them at once, from `Config::store` and then
  from the peers: each listed chunk that the store lacks in one round, and each other
  chunk that the tree needs, one at a time. It reads again once a get gave one of them.
  A get that finds no chunk asks again each second (`RETRY`, on the `env` clock). A get
  asks the voters in key order, without this node, from the voter that gave the last
  chunk, also of an earlier pointer or of a dropped get, and around. A get that a newer
  pointer drops keeps the voter that it was asking, and the next get starts after it.
  One exception: when that voter gave the last chunk, to a get of the same pointer, the
  next get starts at it. Each peer gets the chunks
  still missing in calls of at most `CHUNKS`, each within `peer::DEADLINE`. A call that
  fails or passes the deadline gives the rest of the round to the next peer. Lost:
  holders first; a set of the peers that failed; the get of one listed chunk at a time.
  It dials as `Mesh::apply` does, with the same redial. A failed call of this node's
  store ends the get with `Cause::Blob`. Each other failure of a peer moves to the next
  peer. When no peer gives the chunk, the cause stays
  `Missing`. A failed store call or file call is read again each second. The state is at
  most two trees: the chunks of the spec in use, which `spec::region::definitions` gives
  (REGION CHECK; `laptop.architect-2`, 2026-10-08T16:16:05Z,
  https://github.com/synnaxlabs/foundation/pull/1891#issuecomment-6064178995), or
  `spec::region::tree` of the founding definitions, and the chunks of the tree of the
  newest committed pointer that the task got. A newer pointer drops the chunks of the
  one it replaces. The base tree of `Mesh::apply` comes from `Config::store` only, never
  from the chunks of the spec in use. The pointer in use is one empty file
  `<version>-<root>` in `<Config::dir>/spec`. A spec takes effect after the create and
  the sync of the directory of its file. After a stop, a spec whose file became durable
  takes no effect in that run, and the next open uses it (MESH SURFACE;
  `laptop.architect`, 2026-10-11T16:47:07Z:
  https://github.com/synnaxlabs/foundation/pull/2422#issuecomment-6111328950). The node
  then removes each other file in the directory, as `Mesh::open` does, which keeps only
  the highest version. A failed removal is not a cause: the next change or open removes
  the file. At an open, the task skips each replayed pointer at or below the one that it
  knows. This is correct while the replay passes through the pointer of the file, and
  Raft never drops a committed entry. Trigger: before a forced takeover (K5) can drop a
  committed entry, its issue decides how the task meets the new history. `Mesh::open`
  reads the spec of the highest version that a file names from the store, or uses the
  founding spec when there is no file. A spec that does not read or has problems is not
  an error of the open: `behind` gives the cause. A file that does not name a pointer
  gives `Error::Stray`, and a failed file call `Error::Files`. The restart read gets one
  missing chunk for each run of `definitions`, so it costs time squared in the chunks of
  the tree. Trigger: before a milestone opens a node on a spec of more than `CHUNKS_MAX`
  chunks, `spec::tree` gets one level of a tree at a time (#1231). Decided by
  `laptop.architect`, 2026-10-08T11:03:36Z
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
  https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6061345871. The get
  from the peers: the plan of #1231, `laptop.architect`, 2026-10-19T17:02:16Z
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6272668202), and
  items 2 and 3 of `laptop.integrator-1`, 2026-10-19T18:23:21Z
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6274225327), which
  need no OK. The get at once after a miss, 2026-10-19T18:23:58Z
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6274236848), which
  supersedes the wait of `RETRY` before each get of
  https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6061345871. The
  order of the voters, the get of the listed chunks in one round, and the rest of a
  round to the next peer, `laptop.architect`, 2026-10-20T03:00:33Z
  (https://github.com/synnaxlabs/foundation/pull/3495#issuecomment-6282311269), which
  supersedes the "Also lost" line of the plan
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6272590081) for the
  listed chunks, and the text of item 1 of
  https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6274236848. The
  start after the voter of a dropped get, the read once a get gave a chunk, and the Lost
  line, `laptop.architect`, 2026-10-20T06:46:57Z
  (https://github.com/synnaxlabs/foundation/pull/3495#issuecomment-6285124027), which
  supersedes the start "from the voter that gave the last chunk of the pointer" of
  https://github.com/synnaxlabs/foundation/pull/3495#issuecomment-6282311269. The start
  at a dropped get's voter that gave the last chunk of the same pointer,
  `laptop.architect`, 2026-10-20T07:16:31Z
  (https://github.com/synnaxlabs/foundation/pull/3495#issuecomment-6285554859), which
  supersedes the keep of each dropped get's voter of 6285124027.
