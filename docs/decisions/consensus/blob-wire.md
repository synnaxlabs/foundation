- **BLOB WIRE (#1227)** The blob protocol (protocol 4 of the header) runs on one stream
  from a requester to the server that holds the store. After the header, the requester
  sends gets and puts, and the server sends replies. Fields are little-endian. A get is
  kind 1 and then one or more digests of 32 bytes; the message length gives the count,
  so a get has no count field and no run state, and a requester with more digests than
  one message holds sends more gets. A put is kind 2, the digest, and the length of the
  chunk (`u32`), 37 bytes; its body follows. A reply is kind 1 (chunk: the digest and
  the length, 37 bytes; its body follows), kind 2 (absent: the digest, 33 bytes), or
  kind 3 (stored: the digest, 33 bytes). A body is the bytes of the chunk as stream
  messages back to back with no prefix, each at most the peer's `message_bytes_max`, so
  a chunk larger than one message goes in parts. No message of a body is empty, the body
  starts a new message, and a chunk of 0 bytes has no body message. Each decoder
  (`wire::blob::Server` for the requester's messages, `wire::blob::Requester` for the
  server's) takes from its caller the most bytes a chunk may have, refuses a longer
  chunk at its length field, and refuses a body message longer than the rest of the
  body; `body` gives where in the chunk the next body message starts. The receiver, not
  `wire`, checks the digest over the whole chunk. Stop codes: 16 `MISMATCH` (the bytes
  do not hash to the digest), 17 `TOO_LARGE` (the chunk is longer than the largest block
  of the node), 18 `FULL` (a put would leave the disk under the free floor of the
  store). Each other break is `wire::header::MALFORMED`, and a stop ends each open
  request of the stream. Order is a rule: the server answers requests in order, the
  digests of a get in message order and a put after its body. The requester keeps a
  queue of its open requests, and a reply that does not answer the oldest open request
  is `MALFORMED`. Each reply names its digest. The check lives in `blob` (#1229), and
  `wire` keeps no queue. The sides are `Requester` and `Server`, and the decoded
  messages `FromRequester` and `FromServer`. Lost: a count field in the get (the length
  gives it); a run state for the get as the hub keys have (a get is one message); a
  length prefix on each body message (the stream frames it); a digest check in `wire`
  (the decoder sees parts, and the receiver has the whole chunk). Decided by
  `laptop.architect` (2026-10-07T20:43:10Z):
  https://github.com/synnaxlabs/foundation/issues/1227#issuecomment-6046483057.
  `wire::blob::Error::code` gives the stop code of each decode error: `TOO_LARGE` for
  `TooLarge`, and `MALFORMED` for each other, so `blob` holds no copy of the map.
  Decided by `laptop.architect` (2026-10-08T00:54:19Z):
  https://github.com/synnaxlabs/foundation/pull/1626#issuecomment-6049910210. A
  requester that refuses a chunk over its own limit stops the stream with `TOO_LARGE`,
  the true cause, and the server ends the open requests as after any stop. Supersedes,
  for a get, the reason "the sender goes to another peer" of `TOO_LARGE` in
  https://github.com/synnaxlabs/foundation/issues/1227#issuecomment-6046483057. A
  `TooLarge` or a `MISMATCH` on a get is the failure of that peer for that digest. A get
  of `blob` names one peer and gives back each digest that the peer did not give, with
  its cause: absent, `TooLarge` with the length and the limit, or `MISMATCH`.
  Supersedes, for a get, the words "the call gives the exact error" of the rules in
  https://github.com/synnaxlabs/foundation/issues/1229: the get gives that digest back
  with its exact error and goes on, the stream stops, and nothing is stored. `blob` asks
  for the other open digests again on a new stream to the same peer. `mesh`, which picks
  the peer, asks the next peer that holds the digest, each peer at most once for one
  fetch. When no peer remains, `mesh` fails the fetch with an exact error that names the
  digest and the last cause. The length in a chunk reply is a claim until the body
  hashes, and a member node can lie (`docs/security.md`), so one peer cannot deny a
  chunk. Lost: give up at the first `TooLarge` (one member decides it); a get of `blob`
  over a list of peers (the choice of peer needs the membership, which `mesh` has); a
  get that fails as a whole at the first refused chunk (`mesh` cannot tell which digests
  the peer still owes). Decided by `laptop.architect` (2026-10-08T06:24:02Z and
  2026-10-08T06:29:55Z):
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6053784863 and
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6053878785.
  Each decoder reads only heads. `Put::body` and `Reply::body` give a
  `wire::blob::Body`. The caller counts the body with it, and decodes the next message
  as a head once the body ended. `take` refuses an empty message and a message longer
  than the rest, `remain` gives the bytes to come (0 for absent and stored, and the next
  message is a head), and `end` gives `Error::Unfinished` (`MALFORMED`) when the stream
  ends with bytes to come. This is the shape of CLIENT HELLO, and the counter is one
  crate-private type in `wire::common::body` that the hub bodies also use. Supersedes
  the body check, `body` of each decoder, and `FromServer` of
  https://github.com/synnaxlabs/foundation/issues/1227#issuecomment-6046483057.
  `FromRequester` holds only gets and puts. Lost: decoders that give a head or a body (a
  head-or-body enum where the caller knows which comes, lost in CLIENT HELLO); one
  public body type for `hub` and `blob` (each caller matches a third error type, and the
  texts of `hub::Error` change). Decided by `laptop.architect` (2026-10-09T01:57:16Z):
  https://github.com/synnaxlabs/foundation/issues/1681#issuecomment-6072682474, under
  the OK of 2026-10-08T16:52:55Z:
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697.
