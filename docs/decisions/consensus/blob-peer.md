- **BLOB PEER (#1229)** `blob::peer` runs BLOB WIRE between this node's store and a
  peer, over one stream per call. Until PR 2 of #1229 adds the requester (`peer::get`,
  `peer::put`), `serve` is its one function. `peer::serve(&Store, Incoming)` answers
  each get and stores each put of a stream whose header the caller read, in order, until
  the peer's half ends, and then finishes its own half. It reads heads with
  `wire::blob::Server` at the limit `Pool::largest` of the store. A get gives
  `Reply::Chunk` and the body as messages of at most `Sender::bytes_max`, each a range
  of the block, so no byte is copied, or `Reply::Absent`. A put copies its body into one
  block of the chunk's length and calls `Store::put`, so a put holds one chunk in
  memory, and `Reply::Stored` goes only once the chunk is durable. A get holds the chunk
  that it sends. `transport` also holds each chunk whole while noq keeps a sent byte of
  it (STREAM WIRE), so a get can hold more than one chunk. The bound is `transport`'s
  (#2361; `laptop.architect-2`, 2026-10-11T06:02:25Z,
  https://github.com/synnaxlabs/foundation/pull/2356#issuecomment-6106063839): a session
  holds at most `window_bytes` of sent bytes, and `transport` copies a part of at most
  1452 bytes and holds no block for it. So the chunks that `transport` holds for a
  session have at most `window_bytes` bytes together, plus two chunks for each stream.
  They are at most `window_bytes / 1453` chunks, plus two for each stream
  (`laptop.architect-2`, 2026-10-11T17:39:39Z,
  https://github.com/synnaxlabs/foundation/issues/2361#issuecomment-6111809297;
  `laptop.architect`, 2026-10-11T20:52:09Z,
  https://github.com/synnaxlabs/foundation/pull/2439#issuecomment-6113624952). A
  refusal stops the stream with its code: `wire::blob::Error::code` for a decode error,
  `MALFORMED` for a one-way stream (`peer::Error::OneWay`), `MISMATCH` for bytes that do
  not hash to the digest, and `FULL` for `Error::Floor` and a full disk. Each other
  store error is the failure of the server, not of the peer: both halves end with code
  0, which the requester reads as a dropped stream. The free floor:
  `Config::floor_bytes`, and `Store::put` gives `Error::Floor` when `Files::free` less
  the chunk's length is under it. A put over a file of the chunk's length takes no new
  room, so the floor does not refuse it: a node whose disk is under the floor still
  starts, and puts its founding chunks again (`laptop.architect`, 2026-10-11T05:27:01Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6105825342). A disk
  that is under the floor takes no new file, also not the empty chunk
  (`laptop.architect`, 2026-10-11T10:37:28Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6108145440). One
  check in `Store::put` covers the puts of a peer, the chunks that a get stores, and
  local puts. The floor bounds what a peer may fill and is not a reservation: the room
  of the other puts in flight is not counted, so the overshoot is at most one chunk for
  each put in flight. A put is in flight for a check while its caller holds its future,
  and also when its future drops after the `free` of that check started
  (`laptop.architect`, 2026-10-11T16:58:32Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6111434083;
  2026-10-12T16:10:17Z:
  https://github.com/synnaxlabs/foundation/pull/2432#issuecomment-6128044346, which
  narrows "A dropped put is not in flight" of 6111434083 to a put that dropped before
  the `free` of the check started), as ENV SEAMS bounds the wait of `Files::free`
  (`laptop.architect-2`, 2026-10-11T17:05:00Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6111493387;
  2026-10-12T15:52:54Z:
  https://github.com/synnaxlabs/foundation/issues/1186#issuecomment-6127761370). The
  check does not sync the directory, and `Files::remove` counts the room of a removed
  file as used until `sync_dir` on its directory ends. So after a put over a file of
  another length that stopped before its sync, the floor refuses a put early by the
  room of that file, until the next put that stores a chunk (`laptop.architect`,
  2026-10-11T05:39:27Z:
  https://github.com/synnaxlabs/foundation/pull/2356#issuecomment-6105909022). `node`
  gives `node::Settings::floor_bytes`, 1 GiB by default (`laptop.architect-2`,
  2026-10-12T15:50:49Z:
  https://github.com/synnaxlabs/foundation/issues/1662#issuecomment-6127728083), as
  `laptop.architect-2` asked (2026-10-09T03:49:41Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073881818). Lost: a
  `Peer` handle that keeps one long stream and pipelines each request (a shallow
  wrapper, and a join of send and receive to keep both windows from filling);
  `Store::serve`, `fetch`, and `push` methods (the names collide with the local `get`
  and `put`); the floor only in `serve` (a get and a local put also fill the disk); a
  `sync_dir` and a second `Files::free` before the floor refuses (more file calls and
  their tests, for an error on the safe side); a read of the file's length before each
  put (one more file call on the common path); the list of the open as the source of the
  length (it holds no length, and the file can change after the open), the last two
  approved by `laptop.architect` (2026-10-11T06:17:38Z:
  https://github.com/synnaxlabs/foundation/pull/2356#issuecomment-6106171114);
  `recv_into` straight into the chunk's block (a message longer than the rest then fails
  in `transport`, so `blob` would copy the body rule of `wire`). Decided by
  `laptop.architect` (2026-10-09T03:30:24Z):
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073693777, on the
  plan https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073655130.
  Amended (`laptop.architect`, 2026-10-11T05:27:01Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6105825342): the
  floor counts new room only, and the bound on the chunks in memory names `transport`.
  Supersedes the check that counts a listed chunk as new room, of
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073693777, and the
  rule "Each stream holds at most one chunk in memory" of #1229.
