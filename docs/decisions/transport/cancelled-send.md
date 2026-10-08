- **CANCELLED SEND (#68, 2026-10-07)** A `stream::Sender::send` or `send_parts` future
  that drops after the stream sent a byte of its message (its header counts), and
  before it completes, resets the stream with `Code(0)`. One that drops before that
  sends nothing and changes nothing, and the stream stays open. That includes a drop
  before its first poll, while it waits behind an earlier message, while it waits for
  its turn, and while it waits for room in the send budget. The core takes the message
  out and gives back its room in the send budget and its place in the turn, as a
  completed write does. After a reset, each `send`, `try_send`, `send_parts`,
  `try_send_parts`, and `finish` on the sender gives `Error::Reset { code: Code(0) }`
  after the checks below, and the `Error::Reset` doc names both causes: the peer, or a
  dropped `send` future. A dropped future is a normal cancel in async code, such as a
  timeout in a select, so it must not panic; in both cases the caller opens a new
  stream. Rejected: a panic, as after `finish` (a timeout the caller handles would
  become a crash). Proposed by `box2.builder-5`, decided by the architect, #68
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030986313). Amended
  by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035156093): reset
  only when bytes of the message may have gone. Amended again
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035820204): the
  rule names the fact, a byte went, and not the proxy, the stream took it. `send`,
  `try_send`, `send_parts`, and `try_send_parts` check in this order: the range panic
  (`*_parts`), the panic after `finish`, `Error::TooLarge`, then the state errors
  (`Reset` after a dropped send future, `Stopped`, or the error that ended the
  session). The limit is fixed for the session, so a size defect shows in every state
  of the stream
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035220831).
