- **NODE NAME (2026-10-09)** A data directory holds one node, by name. `Config::name`
  gives it. Under `lock` (DATA DIRECTORY LOCK), after the claim of the shard count,
  shard 0 opens the file `name` with `Mode::Create { len: 277 }`. A file of zeros or
  with the same name gets the name and syncs, and then the data directory syncs, as
  `node.key` does: a failed sync of an earlier start can leave a name that a read sees
  but a crash loses. Another name stops the start with
  `Error::Renamed { stored, given }` (the CLI code is `node.renamed`, #1732). The node
  never writes another name over a name.
  The file is the only copy of the name on the node, and the source of the name that
  the node puts in its card when it founds or joins a region (#1744). In the region,
  `card.name` is the one copy (MEMBER RECORD). Trigger: the first operation that
  renames a node in the region also writes this file, or refuses the rename.
  The file is one sector: the tag `foundation/name/1`, the length of the name in one
  byte, the name with zeros after it up to 255 bytes, and the CRC32C of those 273 bytes
  (little-endian). A crash keeps a sector whole or old, so a file with no bytes or with
  a zero sector is no name. Any other file is a file that no node wrote: the start stops
  with `Error::Name`, and the node keeps the file.
  `node::name(files, given)` gives the name before the start: the stored name, else
  the given one, else `Error::Unnamed`. It opens the file to read only, so it makes
  nothing and waits for no lock. So the first start needs a name, and a later one does
  not.
  Lost: the name in the shard record, because a record is a file name and its count
  changes. Also lost: a text file, because a torn write of text can read as another
  valid name.
  Decided by `laptop.architect-2`, #1732, 2026-10-09T18:30:45Z:
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545.
