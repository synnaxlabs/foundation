- **DATA DIRECTORY LOCK (2026-10-07)** One node at a time uses a data directory. Before
  the claim reads a name, shard 0 opens the file `lock` in the data directory to write
  (`Mode::Create { len: 0 }`), and drops it after each shard of the node has closed its
  ring and each task of the mesh has ended (#585, by `laptop.architect`, 2026-10-08
  03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475), and
  once the transport has freed the port (NODE PORT; #2017, by `laptop.architect-2`,
  2026-10-09 02:17 UTC:
  https://github.com/synnaxlabs/foundation/issues/2017#issuecomment-6072896717). `Busy`
  on `lock` stops the start with `Error::Directory`, before any name is read. The node
  never removes `lock`, so an open cannot race with a remove. A crash frees the lock
  (`env::files`, #392). Lost: no lock, with the `Busy` of each ring only, because two
  nodes with two core counts can each record a count, and the loser's record then
  refuses every later start. Also lost: an atomic claim with no lock, because an
  exclusive create guards one name, and two counts are two names.
  Decided by the architect, #1297:
  https://github.com/synnaxlabs/foundation/issues/1297#issuecomment-6034758419 (#1300).
