- **RECV WAITS (#581, 2026-10-05)** `stream::Receiver::recv` waits while it has no
  block, because the pool has no room or the system refused a commit. It gives the next
  message, `None` at the end, `Error::Reset` when the sender cancelled the stream, or
  the error that ended the session. A full pool and a refused commit get no error:
  `transport::Error` has no `Pool` variant, and the read path's "no room now"
  stays private (architect, #68:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6032721674). Its doc
  says that it waits. The whole message keeps its room in the receive budget, and the
  budget holds the peer (STREAM WIRE). Decided by architect-2 (#1456:
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673). TLS
  over TCP must do the same (TRANSPORT SHAPE LOCKED). One timer for each `Transport`
  retries all of its waiting reads, for both causes; each retry's `alloc` takes back the
  blocks returned since the last try. The retry interval is a `transport` constant that
  simulation tunes (`docs/decisions/open/parameters.md`). The waiting reads of one
  `Transport` take blocks highest class first, then oldest first, so `CatchUp` reads
  cannot starve `Command` reads; other users of the shard pool (M4) are not in this
  order. A read waits for a block only with a whole message that holds its room, so it
  never waits for room in its place. `transport` counts the time that reads wait and
  each refused commit, and `node` publishes them on status channels (BQ11b).
  `Transport::status` gives `Status { waited, refusals }`, pulled, not pushed: `waited`
  is the time that at least one read waited, not the sum over reads (architect, #68:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6032541901).
  `Status` also gives `budget_waits`: each message whose claim queued for room in the
  send budget, over every session, also ended ones; a send that gets room at once does
  not count. It shows that a peer's window limits the sends, and the send bench's
  budget line panics in a round where it does not grow. A count, not a span: a wait
  that ends at the same instant gives a span of 0. Decided by `laptop.architect-2` (PR
  #1952, 2026-10-08 18:30 UTC:
  https://github.com/synnaxlabs/foundation/pull/1952#issuecomment-6066477026, and
  18:32 UTC: https://github.com/synnaxlabs/foundation/pull/1952#issuecomment-6066512702;
  scope approved on #1958, 2026-10-08 18:33 UTC:
  https://github.com/synnaxlabs/foundation/issues/1958#issuecomment-6066530067).
  A caller ends a wait when it drops the future; it can then call `stop`.
  `datagram::Receiver::recv` gives no such error either: a datagram with no block drops
  and is counted, and the read waits for the next one. `hub` writes no retry for a read.
  B5 on the remote hop: the writer's `hub` never waits on a live send. When the stream
  cannot take a live frame now, `hub` drops it and adds its samples and stamps to one
  pending gap for each index. When the stream can take a message again, `hub` sends the
  pending gap first, and the home records it and warns. The writer gets the same answer
  as for a frame the home dropped, and never resends it (B7). Backfill waits. The live
  send is `stream::Sender::try_send` (#597). `block` gets no wake when a block returns
  until simulation shows that the resume latency matters; then `memory` proposes one
  wake, which home backfill shares. Until then, home backfill also retries on a timer.
  Rejected: each caller retries (each caller writes the same timer, and the pool's
  states leak into `hub`), and the stream ends (memory pressure becomes stream churn and
  lost messages, and `Command` streams drop first). Decided by the advisor under the
  delivery and wire internals delegation.
