- **NODE NAME (2026-10-09)** A data directory holds one node, by name. `Config::name`
  gives it. Under `lock` (DATA DIRECTORY LOCK), after the claim of the shard count,
  shard 0 removes a file `name.new` that a crash left, writes the name to a new
  `name.new`, and renames it to `name`, which syncs the file first, then syncs the data
  directory. A file `name` with the same name changes nothing. The claim has synced the
  data directory at this start, which makes good a failed sync of an earlier start:
  that sync can leave a name that a read sees but a crash loses. A read sees no file
  `name` or a whole one, also during a first start. A file `name` that does not hold a
  whole name, also one of zeros or with no bytes, is `Error::Name`: the start stops,
  and the node keeps the file. Another name stops the start with `Error::Renamed {
  stored, given }` (the CLI code is `node.renamed`, #1732). The node never writes
  another name over a name.
  The file is the only copy of the name on the node, and the source of the name that
  the node puts in its card when it founds or joins a region (#1744). In the region,
  `card.name` is the one copy (MEMBER RECORD). Trigger: the first operation that
  renames a node in the region also writes this file, or refuses the rename.
  The file is one sector: the tag `foundation/name/1`, the length of the name in one
  byte, the name with zeros after it up to 255 bytes, and the CRC32C of those 273 bytes
  (little-endian).
  `node::name(files, given)` gives the name before the start: the given one, else the
  stored name, else `Error::Unnamed`. A given name that is not the stored one stops the
  start at the claim with `Error::Renamed`, as another shard count does. It reads the
  file only when no name is given, and opens it to read only, so it makes nothing and
  waits for no lock. So the first start needs a name, and a later one does not
  (`laptop.architect-2`, 2026-10-08T05:00:46Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6052649826).
  Lost: `main` owns the name, and reads and writes the file before `Node::start`. The
  write is outside the lock, so two first starts with two names can leave the name of
  the node that lost, and #1744 founds the region with the name, which `node` must
  then have. Also lost: `Config::name: Option<Name>`, which shard 0 alone resolves.
  The line that `main` prints needs the name, and a task of `Node::spawn` gets only
  the hub, so the node needs a new way to give the name back. Also lost: the same check
  also in `node::name`, because its read is outside the lock, so only the check under
  the lock holds. Also lost: a write in place of the file, which a read during a first
  start can see in part, and a write back at each start. Also lost: a doc that says
  `node::name` can give `Error::Name` during a first start; then `foundation start`
  races to `node.name` in place of `node.busy`. Also lost: `node::name` opens `lock`
  first; that takes the lock that a start needs, and a second start fails.
  Supersedes "A file with no bytes, or with a sector of zeros, is no file", and the
  read of the stored name before the given one, of
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545.
  Decided by `laptop.architect-2`, #1732: 2026-10-09T18:30:45Z, the file and the
  errors,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545;
  2026-10-09T19:54:21Z, the only copy on the node, the source of the card's name, the
  trigger, the given name first, and the first lost designs,
  https://github.com/synnaxlabs/foundation/pull/2172#issuecomment-6088201970;
  2026-10-09T20:09:33Z, the write by rename, a file of zeros or with no bytes, and the
  last two lost designs,
  https://github.com/synnaxlabs/foundation/pull/2172#issuecomment-6088435494;
  2026-10-09T20:42:29Z, a file `name` with the same name,
  https://github.com/synnaxlabs/foundation/pull/2172#issuecomment-6088917341.
