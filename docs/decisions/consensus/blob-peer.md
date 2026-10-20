- **BLOB PEER (#1229)** `blob::peer` runs BLOB WIRE between this node's store and a
  peer, over one stream per call. `peer::serve(&Store, Incoming)` answers
  each get and stores each put of a stream whose header the caller read, in order, until
  the peer's half ends, and then finishes its own half. It reads heads with
  `wire::blob::Server` at the limit `Pool::largest` of the store. A get gives
  `Reply::Chunk` and the body as messages of at most the 1452 bytes that `transport`
  copies, each a range of the block, so `transport` holds no block of a chunk, or
  `Reply::Absent`. A put streams its body into the chunk's file, so it holds no block of
  the chunk (the amendment of 2026-10-18T20:03:51Z below), and `Reply::Stored` goes only
  once the chunk is durable. A get holds the chunk that it sends, and its reply head at
  once. A pool that cannot hold both gives that get `Error::Pool`, as a pool under load
  does, and `serve` ends the stream with code 0 (`laptop.architect`,
  2026-10-18T16:05:19Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6252058410, item 1).
  Lost: a head sent before the read (`transport` still holds the head's block when the
  read takes the chunk's), and a chunk limit that leaves room for one head (the store
  would know the head of its caller). So a get holds one chunk, and the bytes that noq
  keeps are copies inside `window_bytes` (`laptop.architect`, 2026-10-18T17:25:28Z:
  https://github.com/synnaxlabs/foundation/pull/3220#issuecomment-6252885788). Lost:
  messages of up to `Sender::bytes_max` with no copy. Then `transport` holds each sent
  chunk until its ack, so a pool needs up to `window_bytes / 1453` blocks of the largest
  class for each session, a peer that delays its acks pins them, and a get or put of two
  chunks on a pool of one fails (#3232). A refusal stops the stream with its code:
  `wire::blob::Error::code` for a decode error, `MALFORMED` for a one-way stream
  (`peer::Error::OneWay`), `MISMATCH` for bytes that do not hash to the digest, and
  `FULL` for `Error::Floor` and a full disk. Each other
  store error is the failure of the server, not of the peer: both halves end with code
  0, which the requester reads as a dropped stream. `peer::get(&Store, &Session,
  &[Digest])` gets each digest and puts each chunk in the store, and gives each digest
  that the peer did not give as a `Miss` with its `Cause` (`Absent`, `TooLarge`,
  `Mismatch`), in order. Each call opens streams with `Class::Complete`. It sends one
  get of as many open digests as `Pool::largest` of the store and the 1452 bytes that
  `transport` copies both hold, so `transport` holds no block of a get (the
  `Sender::bytes_max` of a peer is at least 1472), and reads each reply before it sends
  the next, so neither side fills the window of the other (`laptop.architect`,
  2026-10-18T17:10:10Z:
  https://github.com/synnaxlabs/foundation/pull/3220#issuecomment-6252724012). It reads
  a body into one block of the chunk's length and calls `Store::put`. A head of the
  oldest open digest over `Pool::largest` of the store, or bytes that do not hash to
  the digest, stops the stream with `TOO_LARGE` or `MISMATCH`, and the digests still
  open go again on a new stream. A reply that does not answer the oldest open digest
  gives `Error::Unexpected`, and an end of the peer's half with digests open gives
  `Error::Unanswered`; both stop with `MALFORMED`. A store error is the failure of the
  requester, so the stream ends with code 0 (the head of the oldest open digest and
  this code: `laptop.architect`, 2026-10-18T16:45:46Z:
  https://github.com/synnaxlabs/foundation/pull/3220#issuecomment-6252474303).
  `peer::put(&Store, &Session, &[Digest])` sends each chunk of the store, one at a time,
  as `serve` sends a get, and waits for its `Stored`. It runs on `peer::Push`, whose
  `open`, `put(self, Digest)`, and `finish` give each step to a caller that bounds each
  one; `put` gives the `Push` back only when its chunk is stored, so none exists after
  an error. `blob` stays free of time (`laptop.architect`, RULING-PENDING:
  https://github.com/synnaxlabs/foundation/issues/3405#issuecomment-6288441342).
  A stop or reset with a code that
  is not 0 is the peer's refusal (`Error::Refused`), and code 0 a failed stream. A drop
  of either future ends the stream with code 0. Each holds at most one chunk of its own,
  and a put holds its chunk and its `Put` head at once, as `Config::pool` states
  (`laptop.architect`, 2026-10-18T16:05:19Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6252058410, items 1
  and 2, on the plan
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6141090295). The
  free floor:
  `Config::floor_bytes`, and `Store::put` gives `Error::Floor` when `Files::free` less
  the chunk's length is under it. A put over a file of the chunk's length takes no new
  room, so the floor does not refuse it: a node whose disk is under the floor still
  starts, and puts its founding chunks again (`laptop.architect`, 2026-10-11T05:27:01Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6105825342). A disk
  that is under the floor takes no new file, also not the empty chunk
  (`laptop.architect`, 2026-10-11T10:37:28Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6108145440). One
  check covers the puts of a peer, before the open of their file, the chunks that a get
  stores, and local puts. The floor bounds what a peer may fill and is not a
  reservation: the room of the other puts in flight is not counted, so the overshoot is,
  for each put in flight, its chunk rounded up to whole blocks of the disk and the
  growth of its directory (`laptop.architect`, 2026-10-18T16:05:19Z:
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6252058410, item
  3). A put is in flight for a check while its caller holds its future,
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
  Amended (`laptop.architect`, 2026-10-18T20:03:51Z:
  https://github.com/synnaxlabs/foundation/pull/3244#issuecomment-6254512129, items 1 to
  3, the plan approved at 2026-10-18T20:10:11Z:
  https://github.com/synnaxlabs/foundation/issues/3251#issuecomment-6254577124, and the
  part file of 2026-10-18T20:42:45Z:
  https://github.com/synnaxlabs/foundation/pull/3258#issuecomment-6254911451; the wait,
  a key choice, accepted by `laptop.monitor`, 2026-10-18T20:08:51Z:
  https://github.com/synnaxlabs/foundation/issues/3251#issuecomment-6254564176): `serve`
  streams a put. The head gives `TOO_LARGE` over `Pool::largest` with no alloc, so each
  stored chunk reads back in one block. A crate-private `Store::receive` then takes a
  slot, checks the floor, and creates a part file of its own (BLOB STORE). Each body
  message is hashed and written at its offset as it comes. The end checks the digest and
  makes the part the durable file of the digest, and `serve` sends `Stored`. No get and
  no local put waits on a receipt while its body comes. A receipt of a digest that a put
  stored since the open also writes a part, and its end removes it (BLOB STORE). A body
  that does not match, and a stream that ends or resets in the body, remove the part, so
  a peer leaves no part of a chunk to fill the disk, and a listed file keeps its bytes.
  A drop of `serve` in a body or in the end of a put leaves the part, and `Store` lists
  its path. Each floor check, of a `receive` or a `Store::put`, first places each kept
  part (below), then removes each listed part, and then syncs the directory when a
  remove of a listed part ended Ok after the start of the last sync that ended Ok, so
  the room of each removed part counts for it. One check at a time does this; BLOB STORE
  gives what its turn covers. A drop of the end in the remove of an earlier file of the
  chunk keeps its part, the only copy of the chunk, and the next floor check, or a put
  of its digest, makes it the file of the digest (BLOB STORE, #3318). After a floor
  check, such a part stays only while a put of its digest is in flight or when its place
  failed, and the store keeps at most one such part for each digest (BLOB STORE).
  Supersedes "leaves its part, the only copy of the chunk, to the next open. The floor
  bounds those parts". Amended (`laptop.architect`, 2026-10-19T00:15:01Z:
  https://github.com/synnaxlabs/foundation/pull/3307#issuecomment-6256938904): the clean
  moved from `receive` to each floor check (#3307). Only a remove of a listed part
  counts (`laptop.architect`, 2026-10-19T00:27:24Z:
  https://github.com/synnaxlabs/foundation/pull/3307#issuecomment-6257075140). A failed
  remove counts in `Status::failures`, and the next open takes the part (BLOB STORE).
  Amended (`laptop.architect`, 2026-10-18T23:52:45Z:
  https://github.com/synnaxlabs/foundation/pull/3244#issuecomment-6256720605): the
  trigger of the list fired, as `node` drops `serve` in a body when the session of a
  member ends (#3244, #3305). At most `PUTS_MAX` (8) receipts of one store are open at
  one time. A head over it waits, with no block and no file, until a receipt drops.
  Heads go in the order they come, so a head that comes again cannot pass one that
  waits. The heads that wait are at most one for each open blob stream, because `serve`
  reads the next head of a stream only after the put before it ends. The bytes of a body
  wait in the window of its stream, which `transport` bounds. A local `Store::put` and
  each get take no slot. The bound also bounds the overshoot of the floor by the
  receipts that are open: at most `PUTS_MAX` chunks. Lost: `blake3` as a dependency of
  `blob` (a second crate that names the hash; `types::digest::Hasher` does it); one
  write of the body at its end (it holds each message in `transport`'s blocks until the
  end); a stop with a new code at the bound (a new code in `wire::blob` and a retry in
  each caller, and an honest peer cannot put in either case while a member holds each
  slot); the read and check of a listed file at the head of a put, in place of the part
  (a get of the digest then waits on the body of a peer); a receipt that only hashes
  when its digest is `Held` at the head (a get can make the digest absent while the body
  comes, and the receipt then has no bytes to store). Supersedes "A put copies its body
  into one block of the chunk's length and calls `Store::put`, so a put holds one chunk
  in memory" of
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073693777.
