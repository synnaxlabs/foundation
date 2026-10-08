- **STREAM WIRE (#55, 2026-10-05)** On QUIC, the side that opens a stream sends one
  class byte first in its own direction: 0 `Command`, 1 `Latest`, 2 `Complete`, 3
  `CatchUp`. The byte goes with the first message, so a stream reaches the peer with its
  first message. The peer queues a stream for accept at the first byte of its first
  message. A stream that ends or resets before that byte drops: the peer never accepts
  it, and resets the reply half of a two-way stream with code 0 (amended:
  https://github.com/synnaxlabs/foundation/pull/1380#issuecomment-6038160446). Each
  message is a QUIC varint length, then that many bytes, at most the receiver's
  `message_bytes_max`. A node accepts the waiting streams highest class first. Each
  stream sends at the QUIC priority of its class, `Command` first, and streams of one
  class share in turn. The priority is strict: a class sends nothing, resends too, while
  a higher class has bytes to send, so a steady higher class starves the lower ones. It
  orders only the bytes that QUIC holds. All classes share one QUIC send window, so a
  message can wait for bytes of a lower class to be acknowledged (#797). The QUIC send
  window, not the send budget, bounds what QUIC holds. A message that QUIC does not take
  in full waits its turn, by class, then oldest first. Only the first sender in turn
  writes, and only it wakes when QUIC has room. A write of a class ahead of every
  waiter goes first; any other write waits, and a `try_send` gives the message back.
  Stream credit is twice the connection window, so a stream never waits on its own
  credit while the connection has room. This relies on reader-granted credits (B3): a
  node takes every byte it granted credit for. A peer that gives less stalls only its
  own connection (#819). The turn goes `Command`, then `Latest` and `Complete` by share,
  then `CatchUp`. While streams of both `Latest` and `Complete` hold a message to send,
  QUIC takes 3 bytes of `Complete` for each byte of `Latest`, within about one window:
  `Complete` goes ahead while it is owed bytes. A class competes while a stream of it
  holds a message, and for one peer window of the other class's bytes after QUIC took a
  byte of it or after a `try_send` of it was given back. A class that competes alone
  makes no debt and no credit, and pays off what it owes or is owed. A class is owed at
  most one peer window of `Latest` bytes. A class that holds less than its share when
  QUIC gives room sends what it holds first, and the core holds no QUIC room for its
  later messages. So one `Latest` stream on `try_send` sends at most one message for
  each step of credit. Room that a stream got and its caller has not taken counts for
  neither class, and a message that `try_send` gave back is not held. The caller of
  `send` writes the rest of its message; the stream writes the rest of a message from
  `try_send` or of a finished stream. A rest that the stream held for a waiting caller
  would take each freed byte before the other class's caller wakes. The send budget
  gives room in the order of the turn. While the owed class competes and a claim of
  the other class holds room, room that a message of the owed class frees waits for
  that class's next message, and the budget starts no new message of the other class,
  so neither class can take the share through the budget (#819). A change of this
  share changes the share bound of `transport/benches/send.rs` in the same PR. Decided
  by architect-2 (#977, 2026-10-07 17:15 UTC):
  https://github.com/synnaxlabs/foundation/issues/977#issuecomment-6042983190. The
  competition memory and the cap: architect-2 (#1311, 2026-10-07 11:14 UTC and 12:09
  UTC): https://github.com/synnaxlabs/foundation/issues/1311#issuecomment-6036747607
  and https://github.com/synnaxlabs/foundation/issues/1311#issuecomment-6037588359. Who
  writes the rest: architect-2 (#1311, 2026-10-07 17:18 UTC):
  https://github.com/synnaxlabs/foundation/issues/1311#issuecomment-6043036616. The
  admission of new messages: architect-2 (#1311, 2026-10-08 05:49 UTC, and #1998,
  2026-10-08 21:12 UTC):
  https://github.com/synnaxlabs/foundation/issues/1311#issuecomment-6053290214 and
  https://github.com/synnaxlabs/foundation/pull/1998#issuecomment-6069147833. Lost: a
  connection per class, because four handshakes and four congestion controllers
  compete on one path (#55). Settled by the advisor and the coordinator under the
  person's delegation (#789).
  A node resets a stream with the stop's code when the stop arrives, and frees
  the stream's room in the send budget and its turn (#1308). A stop that arrives after
  the peer acknowledged all the data of a finished stream, or this side's reset of the
  stream, has no effect, and the node does not check its code, because the carrier has
  freed the stream. Decided by architect-2 (#1445, 2026-10-07 17:48 UTC):
  https://github.com/synnaxlabs/foundation/issues/1445#issuecomment-6043565271.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1445#issuecomment-6042123544. A peer
  breaks the protocol when it sends another class byte, ends a stream inside a message,
  sends a message over the limit, or resets or stops a stream with a code over 32 bits.
  The node then closes the connection with application code 2^32 and the reason as
  text, and the caller gets `Error::Broken`.
  Each connection keeps two budgets, which count the length of each message. A sender
  starts a message only when the messages it started and the streams have not taken in
  full stay within the peer's `window_bytes`; else the write waits for `Writable`. A
  message starts only when it fits and no stream of its class or a class ahead of it
  waits for room. Room that frees goes to the waiting streams by class in that order,
  then oldest first, until the next one does not fit, and only those streams wake
  (#611). A send that does not wait (`try_send`) starts a message only by the same rule
  and when, after a flush, the stream holds no part of an earlier one; else it gives the
  message back with no byte of it sent, and the stream does not wait for room (#597). A
  receiver takes room for a message by the same rule, highest class first, within
  `window_bytes` plus `message_bytes_max`; else the read waits for `Readable` (#611). It
  holds the bytes of a message outside the pool, and takes a block only when the message
  is whole. Decided by architect-2 (#1456:
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673). No
  chunk of the carrier outlives the read that took it: a read that ends before its
  message has a block copies the bytes it holds into one buffer of the message's
  length, outside the pool. Decided by `laptop.architect-2` (#1456, 2026-10-07 17:05
  UTC: https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6042785777).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6042336187 and the
  copy cost of
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673. A
  test asserts that each read leaves no view of a chunk, and that each reader holds
  at most one buffer, whose capacity is the length of its message (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6043389350). So
  bytes that wait for a block never use up the credit that a started message needs,
  and a peer that breaks the send rule holds at most the receive budget and stops only
  its own connection. Each node's first one-way stream is its hello, with no class byte:
  (id, value) pairs, both QUIC varints, ids strictly increasing, then the stream end. Id
  0 is `window_bytes` and id 1 is `message_bytes_max`; both are required. A node ignores
  an id it does not know, so an advisory field needs no new ALPN; a field that the peer
  must understand needs one. The acceptor sends its hello at 0.5-RTT, once it has the
  whole ClientHello and so the peer's transport parameters, or at its `Connected` when a
  HelloRetryRequest holds them back. The dialer sends at its `Connected`. So the hello
  adds no round trip. The hello has its own one-way stream: a node lets the peer open
  `streams_max` + 1 one-way streams, and does not give back the credit of the peer's
  hello stream when it ends, so after the hello the peer has at most `streams_max` open.
  Until the peer's hello arrives, a node opens and accepts no stream; the caller bounds
  that wait, with its other limits before admission (#563). A sender obeys only the
  peer's values: each message is at most the peer's `message_bytes_max`, and the send
  budget is the peer's `window_bytes`. A value over what the node can count counts as
  the largest it can count. A peer breaks the protocol when its hello ends inside a
  pair, misses a required id, has an id out of order, is over 256 bytes, has a
  `message_bytes_max` below 1472 (architect, #1198:
  https://github.com/synnaxlabs/foundation/issues/1198) or a `window_bytes` below it, or
  resets. A peer whose QUIC transport parameters cannot take this node's whole hello at
  once (no one-way stream, or a stream or connection window under the hello) also breaks
  it, with the reason `a peer with no room for the hello`. A dial that breaks so gets
  `Error::Broken` with no `Connected` before it, and an accept gives the caller no
  event. Before the handshake is confirmed, QUIC gives the peer no reason, only
  APPLICATION_ERROR. A Foundation node always has room: `streams_max` is at least 1, and
  `window_bytes` is at least `message_bytes_max`, which is at least 1472. A compile-time
  assertion holds 1472 at or above the hello limit, so only a foreign peer gets this.
  Lost: send the hello later when credit comes, because `open` then needs a second gate
  and a state that only a foreign peer reaches. `Endpoint::write` gives
  `Error::TooLarge` for a message over the peer's limit; a caller that forwards a
  writer's frame gives the writer `Large`, and the writer splits the frame (LARGE
  FRAME). Proposed by `network` in #55; approved by the coordinator on PR #407. The
  budgets: proposed by `network` in #228. The room order: approved by the advisor on
  #611. The hello: proposed by `network` in #55; settled by the advisor and the
  coordinator under the person's delegation (#55). A sender can send one message from
  parts of one block (`send_parts`, `try_send_parts`), and the budgets count it as one
  message, of the sum of its parts. A `stream::Part` is a range of the block, then at
  most 255 zeros. The stream never sends a byte of the block outside the ranges, because
  those bytes can hold stale data of another channel; the padding is zeros, which `hub`
  computes from FRAME LAYOUT. Lost: a range that runs past the series, because it sends
  stale block bytes; a pad rule in the stream, because it puts the hub layout in
  `transport` and is wrong for a series split across messages (architect, #1197:
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032606575, after
  HUB WIRE
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032579333). The
  stream sends the zeros from one static constant of 255 zero bytes, and `Part` holds no
  invariant, so its fields are public. Lost: the cap of 7, because it is FRAME LAYOUT's
  alignment inside `transport` and adds a panic; private fields and a fallible
  constructor for that cap. `stream::Sender::bytes_max` gives the peer's
  `message_bytes_max`, which the hello gives before any stream opens, and `hub` cuts
  each run at it. Lost: a `send_parts` that cuts a run into messages, because the stream
  knows no key or end of HUB WIRE and `try_send_parts` could then send part of a run; a
  probe with `TooLarge`, a guess with one failed call for each session (architect,
  #1197: https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6033870280).
  A receiver can receive into its own buffer (`recv_into`). A message longer than the
  buffer gives `Error::TooLarge` and stays queued, and so does a message whose future
  drops; HUB WIRE makes that `TooLarge` a broken session, not a size probe. Lost: the
  `Message` type of the proposal, because it changes `send` and `try_send` for each
  caller and must own its ranges (architect, #1197:
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032529738).
  The message that `TooLarge` keeps also keeps its room in the receive budget. The
  caller reads again with a buffer that fits, or ends the session. Decided by
  `laptop.architect-2` (#68, 2026-10-08 16:00 UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6063890182).
  `send_parts` gives the carrier one slice of the block for each run of adjacent parts
  over 1452 bytes. It copies each stretch of shorter runs and zeros between them. The
  write reads the caller's parts, and the stream keeps only the parts that the carrier
  did not take, the first one cut at the first byte not taken, in a list that keeps its
  capacity. Lost: a list of slices and stretches built for each message, because it
  costs each part on each send. Decided by `laptop.architect-2` (#68, 2026-10-07 19:01
  UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6044783047, and
  2026-10-07 20:44 UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6046501911). The
  block's count changes once for each run over 1452 bytes. A short run changes no count.
  Decided by `laptop.architect-2` (#68, 2026-10-08 07:29 UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6054932643). A
  stretch that is only the last part, of at most 1452 bytes with no zeros, goes to noq
  from the block. Each other stretch of at most 1452 bytes goes to noq from the buffer,
  and noq copies it in the same `write` (design point 2 of 6044783047). A longer one is
  copied into a new buffer of its length, which noq keeps until the ACK. A partial write
  of it keeps the rest and copies nothing again. Lost: writes of at most 1452 bytes,
  because noq-proto allocates about 3 times for each segment that they fill (1.87x
  copy-then-send and 13 allocations over `send` for 1000 ranges of 8 B); and writes of
  at most 16 KiB, because each byte of a longer stretch is still copied twice, a cut
  copies up to 16 KiB again, and no source gives the 16 KiB. Decided by
  `laptop.architect-2` (#68, 2026-10-08 07:46 UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6055243052). The
  buffer is the connection's and keeps its capacity, which grows by doubling with the
  longest walk (a stretch and at most 1452 bytes of the run after it), under twice the
  peer's `message_bytes_max`. The stretch goes into it in one walk of its parts.
  Lost: a walk that sizes the stretch, then a walk that copies it into a new buffer of
  its length, because the second walk costs more than the second copy (2.31x
  copy-then-send for 1000 ranges of 8 B); a copy into a new `Vec` as the walk goes,
  because the `Vec` grows, or takes the rest of the message and noq holds the extra
  capacity until the ACK; and a last long run that takes the block, because it gave no
  time gain on box2 and adds a case. Decided by `laptop.architect-2` (#68, 2026-10-08
  08:13 UTC, one walk:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6055676016; and 08:25
  UTC, the connection's buffer:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6055859489; and
  2026-10-08 13:26 UTC, its capacity:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6060897050, which
  supersedes the capacity in the 08:25 UTC ruling; worded at 13:57 UTC:
  https://github.com/synnaxlabs/foundation/pull/1879#issuecomment-6061486171).
  `send` and `try_send` write one part, the whole block, through the same write. One
  whole part with no zeros skips the sum and the walk of the parts, and keeps the same
  cut, list, wait, and reset. `send` and `send_parts` poll the carrier through one
  private future, not one through the other. Lost: a second write path for `send`,
  because two paths must stay in step on budget, turns, and resume. Decided by
  `laptop.architect-2` (#68, 2026-10-08 06:32 UTC:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6053922488).
