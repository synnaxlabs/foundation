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
  gap's length is the count of samples lost (B2). An open of a full ring frees the
  oldest record for its restart record, until segments exist (S4). `set_floor` and
  `usage` wait for their first effect, the B1 warning (#1080). Lost: a `trim` call from
  the home (it needs the commit rate, a late trim gives a second gap, and the edge cases
  of the ring move up into `home`). Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6030836762). An open
  frees to the same headroom as a commit. A checkpoint never passes the newest record of
  a path unless a later synced record holds that path's tail (seq and stamp), so a
  restart continues from the disk (A8). That record syncs before the checkpoint, in a
  sync of its own: a crash can keep the checkpoint and lose a record of the same sync.
  The cost of a trim grows with the paths that lose their newest record, not with all
  paths. A carried tail is no sample: a read gives no entry for it, only the gap up to
  it. The trim does not turn on without the carried tail, and the PR that builds it
  records its form on disk here. Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6032697113).
  As built (#1222): the headroom is three times the larger of the largest record and the
  records of the commit that trims, which are the records not yet synced. The space of a
  trim is free only at its release, after its sync. From one trim to the release of the
  next, the ring takes the records of two commits and the blocks that one wrap skips,
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
  ring of 1024 blocks after commits of 40 records of one block, a commit of 80 records
  is not refused and a commit of 81 is, and two commits of 60 records are not refused
  and commits of 60 and 61 are. A commit of 20 records of four blocks is refused when it
  wraps, because the wrap skips 3 blocks, and a commit of 19 is not. On a new ring of
  1024 blocks, with records of one block, a steady load of 341 records in each commit is
  not refused and one of 342 is, at its third commit. A commit of twice the commits
  before it is not refused after commits of 256 records of one block, and is refused
  after commits of 257. After commits of 64 records of four blocks, which are a quarter
  of the area, a commit of 128 and the commit of 64 after it are not refused when no
  record from the commit two before the 128 to the commit after it must skip a block at
  a wrap. When a record must skip 3 blocks at a wrap in one of those four, the 128 or
  the commit after it is refused. With one record in each commit, three records and the
  blocks of one wrap skip must fit in the area. Four of the largest record less one
  block always hold them, and a smaller ring can refuse a live write under a steady
  load: with a largest record of four blocks, commits of 2, 4, 4, and 4 blocks get
  `Full` on a ring of 13 or 14 blocks. So `Layout::new` refuses an area under four of
  the largest record (#1276), and a ring file with a smaller area does not open. That
  minimum is for one record in each commit: on a ring of 16 blocks, a steady load of one
  record of four blocks and one of two in each commit is refused at its third commit. A
  trim moves the tail to the boundary after a record of any kind: a wrap record and a
  restart record also end where a tail can go. Steady pressure in the ruling means a
  load whose commits fit the area. The ring size for a real load is the sizing of `node`
  (SHARD DISK), not the minimum of `Layout::new`. Decided by the architect: the headroom
  (#1222, https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6033965557),
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
  2026-10-07T11:26:39Z).
