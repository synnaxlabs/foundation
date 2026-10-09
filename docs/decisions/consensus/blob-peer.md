- **BLOB PEER (#1229)** `blob::peer` runs BLOB WIRE between this node's store and a
  peer, over one stream per call. `peer::serve(&Store, Incoming)` answers each get and
  stores each put of a stream whose header the caller read, in order, until the peer's
  half ends, and then finishes its own half. It reads heads with `wire::blob::Server` at
  the limit `Pool::largest` of the store. A get gives `Reply::Chunk` and the body as
  messages of at most `Sender::bytes_max`, each a range of the block, so no byte is
  copied, or `Reply::Absent`. A put copies its body into one block of the chunk's
  length and calls `Store::put`, so a stream holds at most one chunk in memory, and
  `Reply::Stored` goes only once the chunk is durable. A refusal stops the stream with
  its code: `wire::blob::Error::code` for a decode error, `MALFORMED` for a one-way
  stream (`peer::Error::OneWay`), `MISMATCH` for bytes that do not hash to the digest,
  and `FULL` for `Error::Floor` and a full disk. Each other store error is the failure
  of the server, not of the peer: both halves end with code 0, which the requester
  reads as a dropped stream. The free floor: `Config::floor_bytes`, and `Store::put`
  gives `Error::Floor` when `Files::free` less the chunk's length is under it. One
  check in `Store::put` covers the puts of a peer, the chunks that a get stores, and
  local puts. The floor bounds what a peer may fill and is not a reservation: the room
  of the other puts in flight is not counted, so the overshoot is at most one chunk for
  each put in flight. `node` gives 1 GiB until a setting exists. Lost: a `Peer` handle
  that keeps one long stream and pipelines each request (a shallow wrapper, and a join
  of send and receive to keep both windows from filling); `Store::serve`, `fetch`, and
  `push` methods (the names collide with the local `get` and `put`); the floor only in
  `serve` (a get and a local put also fill the disk); `recv_into` straight into the
  chunk's block (a message longer than the rest then fails in `transport`, so `blob`
  would copy the body rule of `wire`). Decided by `laptop.architect`
  (2026-10-09T03:30:24Z):
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073693777, on the
  plan https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6073655130.
