- **POLL WRITER (2026-10-16)** `connector::poll::Writer` owns the frames of a kind that
  polls: one value of each channel per request, the stamp of the request in each index,
  a frame that leaves out each `None`, one write again of a group that the home refuses
  as `Backwards`, the samples lost when the pool has no block, and the counts of
  `poll::COUNTS` (`in` and `lost`). The kind keeps its loop and its errors, and calls
  the writer as a library. `connector-opcua` and `connector-modbus` write through it
  (`laptop.architect-2`, 2026-10-16T03:23:05Z:
  https://github.com/synnaxlabs/foundation/issues/432#issuecomment-6207353409). `open`
  takes a channel of no fixed width, since the kind learns the sample type only after
  the open, and `write` panics on a value of it (`laptop.architect-2`,
  2026-10-16T03:48:17Z, item 3:
  https://github.com/synnaxlabs/foundation/issues/432#issuecomment-6207638476). `open`
  refuses a channel that is an index with `Error::Config` (`connector.index`): the
  writer stamps each index, and only the open knows that a channel is an index. In
  `connector-opcua` that error comes before the check of the sample types, so it comes
  alone (`laptop.architect-2`, 2026-10-16T04:50:48Z, item 1:
  https://github.com/synnaxlabs/foundation/pull/2817#issuecomment-6208339402). `open`
  panics on a channel named again: the parse of the kind refuses it, with the span of
  its block. `write` panics on a value that is not of the sample type of its channel:
  the kind maps each value by `Writer::types`, so a wrong type is a defect of the kind
  (`laptop.architect-2`, 2026-10-16T04:17:23Z, items 1 and 2:
  https://github.com/synnaxlabs/foundation/pull/2817#issuecomment-6207963206).
  The writer owns one value slot for each channel, and `write` takes the values of a
  frame as an iterator, which fills each slot again: no kind keeps slots, and no value
  of a frame before can stay. A kind that reads a frame in more than one exchange
  never writes a value of a tick before (`laptop.architect-2`, 2026-10-16T15:03:40Z,
  item 1: https://github.com/synnaxlabs/foundation/pull/2860#issuecomment-6218356587).
  The Modbus kind drains `read::Values`, which panics on a slot that no reply filled
  (MODBUS KIND; `laptop.architect-2`, 2026-10-16T14:33:20Z:
  https://github.com/synnaxlabs/foundation/pull/2860#issuecomment-6217778354, the
  name, 2026-10-16T14:42:11Z:
  https://github.com/synnaxlabs/foundation/pull/2860#issuecomment-6217947516, and
  final, 2026-10-16T14:48:11Z:
  https://github.com/synnaxlabs/foundation/pull/2860#issuecomment-6218066529). The
  OPC UA kind fails each read in which an answer does not have a value for each node
  of its request, so a good read writes each slot. `write` panics when the iterator
  gives fewer or more values than the channels. `connector-opcua` maps its reads by
  position, with no `zip` over its channels, so a read past its channels panics in the
  kind and is never lost with no count, and the Modbus kind maps its reads the same
  way (the plan, 2026-10-16T06:42:57Z:
  https://github.com/synnaxlabs/foundation/issues/2830#issuecomment-6209736752;
  `laptop.architect-2`, 2026-10-16T05:23:34Z:
  https://github.com/synnaxlabs/foundation/pull/2817#issuecomment-6208721580, and
  2026-10-16T06:44:07Z, condition 1:
  https://github.com/synnaxlabs/foundation/issues/2830#issuecomment-6209753061).
  `poll::Writer` and `status::Writer` each write through `stamp::Writer::write`
  (STAMP), which owns the draft, the stamp of each index, the class of each outcome,
  and the write again. Each gives the bytes of its samples through a fill that gets
  each entry and the stamp of the frame and writes that stamp in each index. Each acts
  on the groups that it gives: `poll::Writer` counts their samples applied or lost,
  and `status::Writer` stages its values again for a group that is not applied, and
  closes on `Removed` or a failed disk (`laptop.architect-2`, 2026-10-20T09:06:36Z:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6287471425, on the
  plan https://github.com/synnaxlabs/foundation/issues/2527#issuecomment-6287263795;
  the fill: 2026-10-20T11:08:01Z,
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6289484340).
  Lost: `status::Writer` writes through `poll::Writer`, which then takes text values
  and a second rule for a pool with no block in its hot path; a `stamp::Writer::write`
  that takes a closure that sends the frame, so that each writer keeps its own draft and
  its own match of outcomes; a `write` that takes a filled `Draft` (the write again
  needs a new stamp in each index, so the caller fills a second draft); the last stamp
  of the index from the hub at the open (another writer on the same index can still
  write ahead between two frames, and it changes `hub`) (`laptop.architect-2`, above); a
  copy of the writer in each kind, which the rule of `Backwards` and of pool loss would
  drift between; a `write` that takes a closure per channel, which reads each value up
  to three times and makes the kind count `bad` and `uncertain` in another pass
  (`laptop.architect-2`, 2026-10-16T04:07:48Z:
  https://github.com/synnaxlabs/foundation/pull/2817#issuecomment-6207854657); a bit set
  of the kept channels beside the values, which each caller keeps in step; an error at
  the open for a channel named again, which the parse refuses first; slots that the kind
  fills through `Writer::values`, which live across frames, so a short fill writes the
  values of the frame before, and a clear of each slot after the send leaves out each
  channel that a short fill did not fill, with no count (the plan and
  `laptop.architect-2`, 2026-10-16T06:42:57Z and 06:44:07Z, above); and one buffer for
  the values of a tick in `poll::Writer`, an option again when a third kind reads a
  frame in more than one exchange (`laptop.architect-2`, 2026-10-16T15:03:40Z, item 1:
  https://github.com/synnaxlabs/foundation/pull/2860#issuecomment-6218356587).
