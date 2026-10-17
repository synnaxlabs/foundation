- **BLOB STORE (#1226)** `blob::Store` keeps chunks by `types::digest::Digest` on the
  node's disk through `env::files`. A put returns only after the chunk is durable. A get
  gives bytes only when they hash to the digest; a chunk that fails the check (a write
  torn by a crash, a bad sector) reads as absent, so the caller fetches it again as for
  any absent chunk, and the store counts each one in a crate-private count: an
  `interface` issue makes it public, with a noun for a name, when the first caller (the
  node's status of its disk) needs it. A get or a put holds at most one chunk in memory.
  `put` borrows its chunk (`&Block`). Layout: one flat directory, one file per chunk
  named by the 64 hex digits of its digest, with the chunk's bytes and nothing else, so
  the bytes are their own check and the layout needs no header, no check field, and no
  rename. A pack file with an index lost: it needs record headers, a scan of every byte
  at open, and compaction for removal. Removal of chunks that no kept root reaches is a
  follow-up. Decided by `laptop.architect` (2026-10-07T17:23:56Z):
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6043124789. A torn
  chunk reads as absent, never as a short chunk, because the bytes are their own check.
  A write to a second name and a rename (`File::rename`, #1503) lost: each chunk then
  has a second name, a crash leaves strays at that name, and the open needs a rule for
  them. Decided by `laptop.architect` (2026-10-07T20:47:57Z), item 1:
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177.
  Supersedes the reason "`env::files` has no rename" of the rules in
  https://github.com/synnaxlabs/foundation/issues/1226 (2026-10-07T06:59:56Z). The open
  lists the directory and trusts no name: a get of a listed digest reads and checks its
  bytes, and a put of one writes it again, because a process crash leaves whole bytes in
  the cache that no sync covers, and a put that trusted a read of them would return
  before they are durable. A put refuses a chunk longer than the largest block of the
  pool before any file call. A put of a digest that a put stored since the open makes no
  file call. A put whose future is dropped stores nothing that a get gives unchecked:
  the next get of the digest reads and checks the file, and the next put writes it
  again. Decided by the builder (#1515,
  https://github.com/synnaxlabs/foundation/pull/1515) and `laptop.architect`
  (2026-10-07T17:47:50Z):
  https://github.com/synnaxlabs/foundation/pull/1515#issuecomment-6043557708. Supersedes
  the read on the first put of a listed digest in the plan
  (https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6042962010) and the
  sentence of item 1 of the 17:23:56Z ruling, "the next put or get of the digest reads
  the file first". Every open makes the directory and syncs its parent, because an
  earlier open can have stopped between the two. A put removes a file of another length
  at its name and writes the chunk. Decided by `laptop.architect`
  (2026-10-07T20:47:57Z), items 2 and 3:
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177. A put
  does not retry a create that gives `Full`: `Files::open` syncs the directory one time
  and opens again (ENV SEAMS, #1622), and a `Full` from `Files::open` is the error of
  the put. `laptop.architect-2` decided the rule of `Files::open`, 2026-10-08T00:45:04Z
  (https://github.com/synnaxlabs/foundation/issues/1622#issuecomment-6049807597).
  `laptop.architect` decided that the put loses its retry (2026-10-11T08:02:51Z,
  https://github.com/synnaxlabs/foundation/pull/2375#issuecomment-6106938767), which
  supersedes item 4 of
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177.
  `Store::put` also keeps a free floor on the disk (BLOB PEER).
  Amended (`laptop.architect`, 2026-10-11T06:20:32Z,
  https://github.com/synnaxlabs/foundation/issues/2362#issuecomment-6106191288):
  `Store::status` gives `blob::Status`, the counts of the store after its open.
  `corruptions` counts the gets that read a file whose bytes did not hash to its digest.
  `failures` counts the gets and puts that gave `Error::Files` other than `Full`. `node`
  pulls it on the shard of the store (STATUS CHANNELS). Supersedes the crate-private
  count of item 3 of
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6043124789. Lost:
  the names `Faults` and `Store::faults` (`transport` and `clock` name the value that
  `node` pulls `Status`); one method for each count (two reads for one pulled value); a
  reader for other threads, as `clock::Reader` (one collector reads the counts, and
  `node` owns where it runs); a cause with each failure (no caller reads one).
  Amended (`laptop.architect`, 2026-10-11T10:40:57Z,
  https://github.com/synnaxlabs/foundation/pull/2389#issuecomment-6108172602):
  `corruptions` counts the times a get made a digest absent because the bytes of its
  file did not hash to it. A get whose read ran into a put of the digest adds none,
  because `env::files` gives unknown bytes for calls in flight on one range. A file that
  a put of this open left before its end adds none, because a dropped put is no fault,
  and `blob::peer::serve` drops a put when its stream ends. A file that a put of an
  earlier open left adds one, as a torn write does: the open lists names and cannot tell
  the two. Lost: each get that read bytes that do not hash counts (the count then rises
  on a sound disk in normal work). Supersedes "`corruptions` keeps its rule" (item 3 of
  https://github.com/synnaxlabs/foundation/issues/2362#issuecomment-6106191288) and its
  doc of the field.
  Amended (`laptop.architect`, 2026-10-11T20:14:21Z,
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6113261777): a put
  marks the file of its digest as left by a put of this open only from its open with
  `Mode::Create`, its first call that can make or change the file. A put that is
  dropped before that call leaves the state of the digest as it was, so a file that a
  crash tore still adds one to `corruptions`. Cost: a put that is dropped during that
  open marks the file, also when the open did not run. Lost: the mark from the start of
  the put, which made a get of a torn file that the put did not change add none, and
  gave a digest with no file the state of a listed file.
  `Store::get` and `Store::put` are not a hot path. Each runs one time for each chunk
  call of `blob::peer` and of `mesh`. A chunk holds bytes of a spec tree or of an
  upgrade binary (BQ1). So a chunk message is a control message, and its rate does not
  grow with the data. Lost: a benchmark of the two. Trigger: a decision that gives
  `blob` bytes whose count grows with the data makes the two a hot path, and its PR adds
  the benchmark. Ruled by `laptop.architect` (2026-10-11T11:49:10Z,
  https://github.com/synnaxlabs/foundation/pull/2389#issuecomment-6108706010).
