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
  at its name and writes the chunk. A create that gives `Full` syncs the directory one
  time and opens again, because `Files::remove` counts the room of a removed file as
  used until `sync_dir` on its directory ends. A second `Full` is the error of the put.
  Decided by `laptop.architect` (2026-10-07T20:47:57Z):
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177.
