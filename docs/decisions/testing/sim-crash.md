- **SIM CRASH (2026-10-05)** `Sim::crash(&node, Crash)` ends each thread of a node
  between runs; a test restarts the node with new threads on the same disk. A `Process`
  crash keeps each file call that ended, and ends each call in flight at the crash, so a
  restart finds no file held (#392), not even by a leaked handle (#535). The blocks of
  each file call of the node go back to their pools, those of a leaked call too (#763).
  A crash of either kind closes each serial port of the node, a leaked one too, and a
  socket or serial port from before the crash panics when it polls. A `Power` crash
  keeps, for each 512-byte sector, its durable bytes or the bytes of any one write since
  then, a write in flight too. A `sync` makes durable the writes that ended before it
  started. A failed `sync` makes each sector keep its durable bytes or those of one such
  write, at random. Where writes in flight at once overlap, a power cut or a failed
  `sync` can keep a part of one of them in a sector (#580). A `sync_dir` makes durable
  the entries at its end. A removed file takes space until the removal is durable. The
  monotonic clock starts again and the wall runs on. `join` on a thread that a crash
  ended panics, because no process joins its own threads after it dies. Built by
  `simulation` in #114, #535, #580, and #763. Amended (2026-10-06, #876): a failed
  `sync` covers each write up to the last one that ended before it started, in the
  order of the writes. As on Linux, these writes stay in the cache, clean: a read sees
  them, and a later write goes over them. A power cut drops them, and at each read or
  write of their sector the cache may drop them, by a coin. A sector with a write that
  no `sync` covered is dirty, and the cache keeps it. Amended (2026-10-07, #1449): a
  `Power` crash keeps the durable entries of each directory, as for a create or a
  remove, so it undoes each rename since the last `sync_dir` of the directory, a rename
  in flight too. A `Process` crash applies a rename in flight, as for other calls.
  Decided by `laptop.architect-2`, #1449, 2026-10-07 14:55 UTC:
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508; the
  text, 2026-10-07 17:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6043326214.
  Amended (2026-10-07, #1264): a `Mode::Create` open in flight at a crash that makes a
  file draws its state. After a `Process` crash the file is whole or has no bytes. After
  a `Power` crash there is no file, or the file with no bytes or whole, with the
  entries of its directory durable, as when the file system commits its journal by
  itself or the `fsync` of the open commits it. The commit acts as a `sync_dir` of the
  directory, so it also keeps each earlier change there, a rename too, and the digest
  holds the drawn state. Lost: a create in two calls, one that makes the entry and one
  that allocates; it doubles the calls of each create, changes the stream of each run,
  and adds a step that `env` does not have.
  Decided by `laptop.architect-2` (2026-10-07T18:31:29Z, the entries of the
  directory at 2026-10-07T19:22:31Z):
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692 and
  https://github.com/synnaxlabs/foundation/pull/1553#issuecomment-6045160531.
  Amended (2026-10-07, #1551): the disk keeps one log, in the order that the calls
  ended, of the creates, removes, and renames that no `sync_dir` of their directory
  covered. A rename is one change. A `Power` crash keeps a prefix of the log. It draws
  the prefix from the files stream only when the log is not empty, and the digest holds
  its length. Each file call in flight takes effect as for `Process`, and the prefix
  decides whether its change stays, except a `sync` or `sync_dir` in flight, which has
  no effect. A `sync_dir` makes durable only the changes of its directory. A journaled
  file system can commit more; `sim` does not, so a missing `sync_dir` shows. A file
  takes space while an entry, a durable entry, a change in the log, or a handle names
  it. Supersedes
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508: a
  `Power` crash undoes each rename since the last `sync_dir`, a rename in flight too.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692: the
  commit of a create in flight. Supersedes
  https://github.com/synnaxlabs/foundation/pull/1553#issuecomment-6045160531: a commit
  makes the entries of the directory durable. A create that a `Power` crash cuts is
  whole or has no bytes, and the prefix decides whether its entry stays. A cut gives a
  state that a journaled file system can reach, or a state that only a missing
  `sync_dir` reaches. Lost: a log for each directory, which gives states that need no
  missing `sync_dir`.
  Decided by `laptop.architect-2` (2026-10-07T19:20:28Z):
  https://github.com/synnaxlabs/foundation/issues/1551#issuecomment-6045125302; the
  calls in flight, 2026-10-08T02:39:46Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6051056642; the text,
  2026-10-08T02:27:20Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6050920514; the order
  that the calls ended, 2026-10-08T02:43:34Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6051095403.
