- **FILE RENAME (2026-10-07)** `File::rename(&mut self, to: &Path)` moves an open file
  to `to`, in the same directory, with no replace. It first syncs the file, so that no
  crash leaves the new name with bytes that were not durable (#1441). After `Ok`, the
  handle names `to` in its errors, and a write open of `to` is `Busy` until the handle
  closes. It renames the file that the handle opened, not whatever path now holds its
  old name: when the old path is gone or holds another file (a remove and a create
  since the open), it gives `NotFound { path: old }` and changes nothing. When `to` is
  there, it gives `Exists { path: to }` and changes nothing; the handle stays usable. A
  read handle, or a `to` that is not a name in the directory of the file (another
  directory, empty, `.`, or ending in `/` or `/.`), is a defect and panics. A trailing
  slash gives `ENOTDIR` on Linux and `ENOENT` on macOS, so no error can name it the
  same way on both. It poisons the file when its sync fails or when it is dropped
  before it ends, as any other call. The rename can still end after the drop, and
  then the file is at `to`. `os` checks that the old path still names the file by
  device and inode, with no follow of a link, then renames with `RENAME_NOREPLACE`;
  the I/O thread of a shard runs its calls in order, one shard writes each name
  (SHARD BUFFERS), and one node uses a data directory (DATA DIRECTORY LOCK), so
  nothing in Foundation changes the path between the check and the rename. Lost:
  `Files::rename(from, to)` on paths, which cannot tell the file of the handle from a
  new file at its path; a link then an unlink, which leaves two names at a crash; a
  replacing rename or a `replace: bool`, which no caller wants and which hides a
  defect that `Exists` reports; and a bare-name `rename(&mut self, name: &OsStr)`: an
  `OsStr` can hold a `/`, so it needs the same check, and it would be the one call
  that takes a name in place of a path in the data directory (#1449, decided by
  `laptop.architect-2`, 2026-10-07 14:55 UTC:
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508; the
  panic list and the bare-name reason, 2026-10-07 17:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6043326214). Also
  lost: a rename that also syncs its directory: it would be the one directory change
  that is durable when it ends, several changes could no longer share one
  `sync_dir`, and a failed directory sync would be a third result, a rename that took
  effect and is not durable (#1503, decided by `laptop.architect-2`, 2026-10-07
  19:11 UTC: https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044972392;
  the race sentence, 2026-10-07 19:12 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044987221).
