- **OPCUA SUBSCRIBE (#2622)** The `opcua` kind subscribes to each node of its reads,
  unless `polled = true`, which reads as OPCUA READ states. A run first reads the
  `CurrentTime` of the server (`ns=0;i=2258`), then subscribes at the sampling of the
  rate, then writes the changes of each drive of the client. It reads `CurrentTime`
  each 10 s, and at once when a server timestamp of a drive restarts the fit of the
  server clock, or when the newest server timestamp of a drive is older than that of
  the drive before, so a clock that goes back shows at the next answer
  (`laptop.architect-2`, 2026-10-14T01:37:20Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229).
  The device time of a value is its source timestamp, or its server timestamp when the
  source timestamp is absent or later. Its mesh time comes through
  `connector::stamp::Fit` (STAMP FIT), and its stamp through `poll::Writer::sample`
  (STAMP). A value whose timestamps the fit cannot place in the range of a `Stamp`
  ends the run with `kind::Error::Config` and `opcua.no-timestamp`, as a value with no
  timestamp does: only a server outside the rules gets there, and `polled = true` is
  the fix. Lost: the time of its arrival, which has no true error (`laptop.architect-2`,
  2026-10-19T23:36:41Z, item 1:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6279727836).
  A value that is not Good and has no timestamp makes no sample, and `bad` or
  `uncertain` counts it, as the polled mode counts a read that is not Good
  (`laptop.architect-2`, item 2 of
  https://github.com/synnaxlabs/foundation/pull/3507#issuecomment-6283684793). Inside
  one drive, the device times of each node never go back: a change whose device time
  is earlier than that of the change of its node before it gets the time of that
  change. Lost, a simple version first: the true time of the values on one side of a
  clock step inside one drive. A fit for each side of the step keeps it
  (https://github.com/synnaxlabs/foundation/issues/3514).
  The queue of each item holds the samples of the larger of two publishing intervals
  that the server gives and two round trips of the first `CurrentTime` read. Past a
  round trip of two intervals, one Publish answer can come a round trip after the one
  before it. Past the queue, `missed` counts each overflow. Lost, a simple version
  first: a resize of the queues with `ModifyMonitoredItems` at each `CurrentTime` read
  whose round trip grew. Trigger: `missed` counts overflows on a link whose round trip
  grew after the connect (`laptop.architect-2`, item 2 of 6279727836).
  `Writer::write` never waits, so the longest wait of a drive, while a change waits in
  the client and no Publish request goes out, is the 5 s timeout of the `CurrentTime`
  read. The client asks for a keep-alive of 1 min, and the lifetime of a subscription
  is at least three keep-alives, so it passes only when no Publish request reaches the
  server for about 3 min. The values that the server samples past the queue in such a
  gap are lost with no count, as the polled mode loses the reads of a link that is
  down (`laptop.architect-2`, item 3 of 6279727836). The end is not silent: the run
  ends with the Device error "the subscription to {address} ended with {status}", which
  the `error` status channel holds at the time of the end, and `restarts` counts it
  (`laptop.monitor`, the condition on the lifetime end of the PR 5 key choices,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6283679034).
  A subscription that ends, or a node that the server does not watch, ends the run with
  `kind::Error::Device` (`laptop.architect-2`, section 3 of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157348272). The
  run closes its session, the supervisor connects again with its backoff, and the next
  run subscribes again, so data starts again after a server restart with no user step
  (`laptop.monitor`, round 2 of #3435, step 4 of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6278830972). The
  polled mode counts a node that the server does not watch as `bad` and writes the
  others. Lost, a simple version first: leave out an item that the server does not
  watch, note it, and try it again at each read of `CurrentTime`
  (https://github.com/synnaxlabs/foundation/issues/3509). A node whose
  CreateMonitoredItems call alone is past one message of the server ends the run with
  `kind::Error::Config` and `opcua.large-watch`, as `opcua.large-read` does for a
  Read.
  When CreateSubscription or CreateMonitoredItems answers `BadServiceUnsupported` or
  `BadNotImplemented`, as a server of the Nano Embedded Device profile can (OPC UA
  Part 7), the kind closes that session and polls on a new session for the rest of
  that connect, as `polled = true` does, so the server keeps no subscription, with the
  note "polls: the server has no subscriptions ({status})". The next connect tries
  a subscription again, since the server can change. Other statuses end the run with
  `kind::Error::Device`. The person ("Ok what you choose is ok", 2026-10-13, through
  `laptop.monitor`, 2026-10-13T23:43:30Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157559393), and
  `laptop.architect-2`, 2026-10-13T23:43:18Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157556938, with
  the note in place of a count `polled` at 2026-10-13T23:48:43Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157622562. Lost: a
  Device error (it never ends); a Config error that asks for `polled = true`; a read of
  `ServerProfileArray` first. The opcua conformance suite runs the subscribed path,
  and only crate tests run the polled path (`laptop.architect-2`, item 4 of
  6279727836).
