- **HTTP SIM SERVER (#1151)** `connector::http::sim::serve(listener, tasks, answer)`,
  behind the `connector` cargo feature `sim`, off by default, is the one HTTP/1.1
  server of the protocol simulators of HTTP connectors. It runs `hyper`'s server on
  each stream, on its own task, with keep-alive, and gives `answer` each request with
  its whole body. A client that closes its write side after a whole request still gets
  the answer. A request that breaks HTTP gets 400, or 414 when its URI is too long and
  431 when its head is too long, and ends its stream; an HTTP/2 preface ends it
  with no answer. Each simulator answers a `content-encoding` in its own route. It
  returns the listener's error, so a test server that cannot accept fails loud. When the
  caller drops the future, the server accepts no more streams, and each stream that it
  accepted continues to run. `hyper`'s server reads OS wall time on each poll (hyper
  1.12.0, `common/date.rs`) only for the `date` header, which is off. A timer reads its
  own `Instant` to arm the header read timeout, which is off too
  (`header_read_timeout(None)`). The person approved the server on the condition that it
  gets no timer. Lost: our own server on `httparse`, which the person refused; and a
  copy of the server in each kind crate.
  Decided by architect-2 (2026-10-07T16:33:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321,
  2026-10-07T16:41:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508,
  2026-10-07T17:08:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042839816,
  2026-10-07T17:14:34Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042958763,
  2026-10-07T17:22:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777) and the
  person (2026-10-07T17:04:29Z
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042756353).
  Supersedes: https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508
  (items 1 and 4, by the person's ruling and 6042839816; item 9, by 6043097777 item 5),
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042839816 (its 400
  and 415 sentences, by 6043097777 items 4 and 5; its `# Errors` section, by
  6042958763),
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321 (the doc
  of finding 1, by 6042446508 item 1 and 6042839816).
