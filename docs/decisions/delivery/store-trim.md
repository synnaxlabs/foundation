- **STORE TRIM (2026-10-06)** Under disk pressure, `buffer` frees its oldest records
  itself, in the commit task, whatever the floors: a ring frees space only at its tail,
  so a floor never changes which record goes (B1). The commit writes the new tail in the
  same sync as its data, and reuses the space only after that sync. A trim never frees a
  record that a read in progress holds (#510). `buffer` keeps its own headroom (at least
  two records of `body_max`, or twice the records of the commit that trims), so a full
  ring does not refuse a live write under steady pressure. `append` gives
  `Rejected::Full` only when the records queued since the last commit do not fit after
  the trim: the full disk queue of B5, which is the commit queue. A read reports the
  trimmed seqs of a path as `Read::gap`, also when the path holds no entry, and the
  gap's length is the count of samples lost (B2). A hub reader gives the seqs lost for
  no room as the gap of its next frame, `hub::reader::Received::gap`, and the seqs that
  a trim freed once it reads from disk (#2723) (#2553, `laptop.architect`,
  2026-10-15T18:12:25Z:
  https://github.com/synnaxlabs/foundation/issues/2553#issuecomment-6199035922). Each
  data record leaves three blocks free: for the restart record of the next open, the
  carry record of the commit after
  it, and the restart record of a later open. Each carry record leaves one block, for
  the restart record. A restart record leaves none, and an open writes over a restart
  record that no data or carry record follows. So an open of a full ring has its
  block, and the commit after it has a block for its carry record, or needs none,
  because a durable carry record before the open holds the tails. This holds after any
  count of opens (#160,
  https://github.com/synnaxlabs/foundation/pull/2683#issuecomment-6177122793,
  2026-10-14T19:58:01Z, which supersedes the two blocks of
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6158992463,
  2026-10-14T01:42:08Z; text of
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6196998191,
  2026-10-15T16:27:31Z).
  `set_floor` and `usage` wait for their first effect, the B1 warning (#1080). Lost: a
  `trim` call from the home (it needs the commit rate, a late trim gives a second gap,
  and the edge cases of the ring move up into `home`). Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6030836762). An open
  frees to the same headroom as a commit. A checkpoint never passes the newest record of
  a path unless a later synced record holds that path's tail (seq and stamp), so a
  restart continues from the disk (A8). That record syncs before the checkpoint, in a
  sync of its own: a crash can keep the checkpoint and lose a record of the same sync.
  The cost of a trim grows with the paths that lose their newest record, not with all
  paths. A carry record that holds one such path also takes the paths with the oldest
  holders after the reach, up to the tails that one record may hold, so that the tails
  of many paths do not each take a record (`laptop.architect`, 2026-10-16T01:50:57Z:
  https://github.com/synnaxlabs/foundation/pull/2688#issuecomment-6206211723, item 2,
  which amends the carry plan of
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6158967741). A
  carried tail is no sample: a read gives no entry for it, only the gap up to it. The
  trim does not turn on without the carried tail, and the PR that builds it records its
  form on disk here. Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6032697113).
  As built (#1222): the headroom is three times the larger of the largest record and the
  records of the commit that trims, which are the records not yet synced, and the three
  blocks that each data record leaves free (#160). The space of a trim is free only at
  its release, after its sync. From a tail at the low mark to the release of the next
  trim, the ring takes the records of two commits and the blocks that one wrap skips,
  which are less than one largest record. So after commits of `c` bytes, the next commit
  fits when it and the skip are at most `2c`, and the next two fit when they and the
  skip are at most `3c`. A load that grows by the factor `g` with each commit fits while
  `g + g²` times `c`, and the skip, are at most `3c`: under about 30 percent a commit.
  Above that, `append` gives `Full`, the full commit queue of B5. A trim cannot free the
  commit in its sync, so the bound also needs an area that holds three commits in a row
  and one wrap skip: `3c` and the skip for a steady load, and `4c` and the skip when one
  commit is `2c`. Lost: twice the records of the commit that trims (it refused a write
  at each wrap), and twice those records plus one largest record (it refused each commit
  that was more than one largest record over the commit before it). Each figure that
  follows is measured with each commit placed while the one before it syncs. On a full
  ring of 1024 blocks after 396 commits of 50 records of one block, with the tail at the
  low mark, a commit of 100 records is not refused and a commit of 101 is. After 398
  such commits, two commits of 75 records are not refused and commits of 75 and 76 are.
  After 57 such commits, a commit of 25 records of four blocks is not refused and one of
  26 is. After 60, the commit wraps and the wrap skips 3 blocks, so 24 are not refused
  and 25 are. On a new ring of 1024 blocks, with records of one block, a steady load of
  340 records in each commit is not refused and one of 341 is, at its third commit. A
  commit of twice the commits before it is not refused after commits of 255 records of
  one block, and is refused after commits of 256. Each of these two figures is one
  record less than before #160, because of the blocks that stay free. On a ring of 1028
  blocks, after commits of 64 records of four blocks, which are a quarter of the area
  less one block, a commit of 128 and the commit of 64 after it are not refused when no
  record from the commit two before the 128 to the commit after it must skip a block at
  a wrap. When a record must skip 3 blocks at a wrap in one of those four, the 128 or
  the commit after it is refused. With one record in each commit, three records, the
  blocks of one wrap skip, and the three blocks that stay free must fit in the area.
  Four of the largest record and two blocks always hold them, and a smaller ring can
  refuse a live write under a steady load: with a largest record of four blocks, commits
  of 4 blocks after one of 1 block get `Full` on a ring of 17 blocks. So `Layout::new`
  refuses an area under four of the largest record (#1276) and three blocks (#160), and
  a ring file with a smaller area does not open. That minimum is for one record in each
  commit: on a ring of 19 blocks, a steady load of one record of four blocks and one of
  two in each commit is refused at its third commit. A trim moves the tail to the
  boundary after a record of any kind: a wrap record and a
  restart record also end where a tail can go. Steady pressure in the ruling means a
  load whose commits fit the area. The ring size for a real load is the sizing of `node`
  (SHARD DISK), not the minimum of `Layout::new`. Decided by the architect: the headroom
  (#1222, https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6033965557,
  with the three blocks of
  https://github.com/synnaxlabs/foundation/pull/2683#issuecomment-6177122793,
  2026-10-14T19:58:01Z),
  the area that the bound needs (#1222,
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6034545693,
  2026-10-07T08:57:53Z, and with the skip in the `4c` case
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6034791932,
  2026-10-07T09:12:31Z), and the boundaries and the minimum of `Layout::new`, built in
  #1276 (#1222,
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6033889998,
  2026-10-07T08:16:51Z). The blocks that a wrap skips are no record, so the headroom
  leaves them out; the bound counts one skip on its own. A release past the last synced
  record panics. Decided by the architect (#1345,
  https://github.com/synnaxlabs/foundation/issues/1345#issuecomment-6036930649,
  2026-10-07T11:26:39Z). The carried tail is a carry record, kind 4 at ring version 1,
  in the form of the carry body in S4. Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6051990962,
  2026-10-08T04:07:38Z). The blocks that stay free and the least area, which move the
  figures above by those blocks, are decided by the architect: two blocks (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6158992463,
  2026-10-14T01:42:08Z), then three, which supersedes two, so that a commit after an
  open of a full ring has a block for its carry record (#2683,
  https://github.com/synnaxlabs/foundation/pull/2683#issuecomment-6177122793,
  2026-10-14T19:58:01Z).
  As built (#2943): a trim has a low mark and a high mark. The low mark is the head
  less the lead, which is the area less the three free blocks and the headroom. A
  commit trims only when the tail is behind the low mark. The trim then frees to the
  first record end at or past the high mark, the low mark once the head moves on by a
  step: the smaller of 100 blocks and half the lead. So a full ring writes one
  checkpoint for each step, not one for each commit. The high mark is the head and the
  step less the lead, and 0 while that is negative, so that it grows from 0 while the
  ring fills. This high mark is approved by `laptop.architect` (#2943,
  2026-10-16T22:49:36Z):
  https://github.com/synnaxlabs/foundation/pull/2943#issuecomment-6226155380. The reach
  of a carry is the high mark of the next trim. The tail still
  never passes `kept`, a read hold, or the last synced record. A trim that a hold stops
  at or past the low mark waits for the next low mark, and a trim that a hold stops
  short of it trims again at the next commit. On a small ring, where the step is half
  the lead, each path is carried about twice as often as with a reach at the low mark
  (S4). Decided by `laptop.architect` (#160, 2026-10-16T03:13:34Z):
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6207224353, which
  approves addendum
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6207213605, and
  the carry rate (#2943, 2026-10-16T22:39:49Z):
  https://github.com/synnaxlabs/foundation/pull/2943#issuecomment-6226036963. The
  refusal figures after 396 and 398 commits and the rise against a reach at the low mark
  are approved by `laptop.architect` (#2943, 2026-10-16T23:14:08Z, item 4):
  https://github.com/synnaxlabs/foundation/pull/2943#issuecomment-6226442923.
  As built (#160 step 3d): the open trims as a commit does, with nothing queued. The
  switch `Config::trimmed` is removed, so the open and each commit trim in each build.
  Decided by `laptop.architect` (#160, 2026-10-14T19:58:03Z):
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6177123224, and
  item 5 of
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6207224353
  (2026-10-16T03:13:34Z).
  After its walk, each open first syncs the header and the records that
  it walked, as read, before it writes a new byte (S4): a failed sync of an earlier open
  or commit in the same boot can leave its checkpoint in the cache only, where the walk
  reads it, and a checkpoint never passes a record that is not durable. With a trim, the
  open then writes the checkpoint of the trim and its restart record, in a block that
  was free before the trim, and one sync makes both durable. Only then does it release
  the trimmed records. A crash that keeps the checkpoint and not the restart record
  opens from the new tail over records that nothing wrote over, and a crash that keeps
  the restart record and not the checkpoint is an open with no trim. When no block is
  free, the open writes the checkpoint alone, syncs, releases, then writes its restart
  record in a freed block and syncs. So an open syncs twice with a trim or with none,
  and three times when no block is free: against the one sync of an open before #2970,
  an open that trims adds one sync, and two when no block is free, as P1 of order (a)
  states. When no block is free and no trim frees one, the open gives `Error::Full` and
  syncs nothing. Decided by `laptop.architect` (#160, 2026-10-14T01:40:03Z, order (a)):
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6158968102, which
  supersedes the order of
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6035839192
  (2026-10-07T10:16:33Z): there, the restart record could go over a freed record in the
  sync of the new checkpoint, and a crash that kept the record and the old checkpoint
  lost the first record of the walk. The first sync of each open, which adds one sync to
  an open with no trim, is from #2970, approved by `laptop.architect`
  (2026-10-17T03:30:04Z):
  https://github.com/synnaxlabs/foundation/pull/2970#issuecomment-6229057387, with a
  failed sync of a commit as a cause and the place after the walk at
  2026-10-17T04:29:55Z:
  https://github.com/synnaxlabs/foundation/pull/2970#issuecomment-6229648982. Lost: a
  first sync only when the restart record goes past the room that the older header slot
  keeps, which keeps one sync for most opens with no trim and adds the tail of that slot
  to `Header::decode`.
