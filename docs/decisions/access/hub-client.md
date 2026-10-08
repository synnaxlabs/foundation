- **HUB CLIENT (2026-10-08)** `hub::client::Client` is the session of a program with one
  node. `Client::connect` dials on a `transport::Client`, sends the subject's hello, and
  gives the client once the node admits it, so a refused hello is an error of `connect`.
  A hello expires at the latest mesh time of its challenge, plus the time since the
  challenge came on the monotonic clock, plus `client::LIFE` (10 minutes), or at the
  end of mesh time when the sum passes it, as mesh time is the node's. A task renews
  the hello at half of `LIFE` after each admission, on the hello stream, until the
  session ends. A renewal waits for a block as each message does, and the node closes
  the session if the hello expires first. A challenge that `wire` refuses ends the
  renewal and closes the session with `MALFORMED`. Each later request gives the error
  that ended the renewal. `request(body)` signs the body, sends it on its own stream,
  and gives the body of the response. Requests of one client go one at a time, in the
  order they began, by a turn in the client, as a link holds one open request (HUB
  LINK). A request dropped before its response began keeps the turn until the response
  begins or the stream ends, because the node holds it open until then. `Config` holds
  its own `pool`, which a program may share with the transport. Each message that the
  client sends takes a block from it, which the stream holds until the node acknowledges
  it. A message that finds the pool full waits for a block, so a small pool makes the
  client slower, not fail. It tries again every 10 ms, the span of the transport's
  reads, and the wait ends when the session closes, with the error of the close. Only
  `Exhausted` and `Refused` wait: `TooLarge` never succeeds, so it gives `Error::Pool`
  at once, which means that no block of the pool can hold a message that the client
  sends. Lost: a bound of two windows from the stream credit, which ack loss can exceed;
  a `Bytes` body with no copy; a wait in `block::Pool` that wakes at each free. Two
  callers poll the pool now: the transport's reads and this client. Trigger: a third
  caller that waits for a block, or a measured request latency from the poll, adds the
  wait to `block`. `Client` is `Clone`, and a clone is the same session. When the last
  clone drops, the client closes the session with `Code(0)`. A node's stop and close
  with a code are one error: `Error::Refused(wire::hub::client::Refusal)`, a closed set
  in `wire` with `from_code` and `code`, of each code that a node stops a client stream
  or closes a client session with (`MALFORMED`, `BUSY`, and `REFUSED` to `CHANGED`).
  Each meaning of a code is stated once, in `wire`, and its `Display` gives the cause. A
  close with 0 or a code outside the set stays `Error::Transport`. Lost: one dial for
  each request (a handshake for each request, and `foundation status` needs a session
  that lives); `&mut self` with the renewal inside `request` (an idle program loses its
  session at the expiry); a queue in a task that owns the session; a lock of a library
  (`tokio::sync::Semaphore` is FIFO, but it is built for many threads, with atomics and
  an `Arc` for an owned permit, where `hub` needs one lock on one thread, and
  `futures::lock::Mutex` wakes the first waiter of its slab, not the oldest, and lets a
  new `lock` take the lock ahead of it, which starves a waiter); `Error::Stopped` with a
  `u32` code, which gives `hub` a meaning for each code. The plan
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6069186434) was
  approved with the `Refusal` change by `laptop.architect` at 2026-10-08T21:25:05Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6069345374).
  `laptop.architect` approved the surface and the shared `pool` at 2026-10-08T21:32:05Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069455056) and
  21:36:08Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069514013), and the
  order "in the order they began" and the dropped request at 21:38:44Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069553753). It
  approved the `MALFORMED` close at 22:04:03Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069924128). It gave
  the reason against `tokio::sync::Semaphore` at 22:15:33Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070115889). It
  approved the wait for a block at 22:56:10Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070683319).
  Supersedes the sentence "Any pool works" of
  https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069455056, and the
  room of the pool approved at 22:04:03Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069924128), at
  22:34:24Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070406709), at
  22:43:35Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070527522), and at
  22:46:20Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070562330).
