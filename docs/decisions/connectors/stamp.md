- **STAMP (#435)** `connector::stamp::Stamps` stamps the frames of one writer of a kind
  that reads by request: one stamp goes into each index of the writer
  (`laptop.architect-2`, 2026-10-16T04:32:46Z, item 3:
  https://github.com/synnaxlabs/foundation/pull/2817#issuecomment-6208137622). The
  stamp of a frame is the midpoint of the mesh times before and after the requests that
  read its samples (`laptop.architect-2`,
  2026-10-14T01:37:20Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229), or
  1 ns after the stamp before it when that is later, so no stamp is at or before the
  stamp before it. The first stamp of a run has no stamp before it, so the home can
  refuse the first frame of a run as backwards when the run before it wrote a stamp at
  or after it on the same index. Each stamp given counts as the stamp before the next,
  written or not (`laptop.architect-2`, 2026-10-13T01:39:14Z, item 4:
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136202010). So "the
  last stamp of the writer" in CONNECTOR STATUS is the last stamp that `Stamps` gave,
  also to a frame for which the pool gave no block (`laptop.architect`,
  2026-10-13T02:15:47Z,
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136738770). After a
  refusal as backwards, the frame is written again 1 ns after the stamp that the
  refusal gives (`laptop.architect-2`, 2026-10-13T01:08:55Z, item 7:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6135871024, and
  2026-10-13T02:18:10Z, item 5:
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136764966).
  `after` panics for a stamp under the last stamp, since a refusal of the frame of the
  last stamp never gives one. `connector::poll::Writer` (POLL WRITER) is the one
  writer that writes a refused frame again inside for each kind that polls. The status
  writer still writes a refused frame again itself; one writer for it is deferred to
  #2527. The status writer of the supervisor stamps its frames with `Stamps::next`
  of its one read of mesh time, so one module holds the stamp rule
  (`laptop.architect-2`, 2026-10-13T01:38:13Z, item 2:
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136191164, approved
  2026-10-13T02:04:57Z, item 3:
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136552780). The
  person put the module in `connector` for each kind that reads by request (ONE NODE
  record, section 3, 2026-10-13T00:50:41Z,
  https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6135595217). Lost:
  the stamp at the answer (it is late by the whole answer time); the module in
  `connector-opcua` (each later kind that reads by request copies it); a method on
  `hub::writer::Writer` (the hub then knows that a request has two ends); the
  nanosecond after the later of the two (it puts the refused frame after a frame from
  a later read); `now` as a closure (a caller can pass a clock that is not mesh time).
- **STAMP FIT (#2622)** `connector::stamp::Fit` fits one device clock to mesh time,
  and `Stamps::sample` stamps a sample from the mesh time that a fit gives
  (`laptop.architect-2`, 2026-10-14T01:37:20Z, PR 2:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229). `Fit`
  holds one `estimate::overlap::Overlap` of 200 ppm, over offsets of mesh time minus
  device time. `between` gives both edges and `before` gives a high edge. An offset is
  computed in `i128`. A low edge under the range of a `Span` is left out, and a high
  edge under it becomes `i64::MIN` ns, so a reading never claims more than it knows
  and no input panics (`laptop.architect-2`, 2026-10-19T14:55:55Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6270224530). A
  property test over any `Monotonic` and mesh time pins it. A `before` older than the newest reading is checked against the earliest
  mesh time that the fit gives at its device time: one under it restarts the fit, and
  else it adds nothing. A refusal as `Backwards` or `Disjoint` of a `between`, or as
  `Disjoint` of a new `before`, restarts the fit from that reading. `sample` takes the
  floor of the midpoint, as `request` does. Lost: `Fit` inside `Stamps` (a writer
  with the values of two devices then needs two `Stamps` for one index, so a stamp can
  go back); a fit that keeps each reading and solves a line (it allocates, and its
  bound is not hard).
