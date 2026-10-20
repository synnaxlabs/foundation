- **STAMP (#435)** `connector::stamp::Writer`, crate-private, stamps the frames of one
  writer: the `poll::Writer` of a kind and the status writer of the supervisor each
  hold one, and no other code gives a stamp. It keeps the rule in a private value with
  no hub, so the tests of the rule need no node (`laptop.architect-2`,
  2026-10-20T13:14:36Z:
  https://github.com/synnaxlabs/foundation/issues/2527#issuecomment-6291709126). Lost:
  the rule merged into `stamp::Writer` (each case of the property tests then opens a sim
  node). It stamps the frames of a kind that reads by request: one stamp goes into each
  index of the writer (`laptop.architect-2`, 2026-10-16T04:32:46Z, item 3:
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
  last stamp of the writer" in CONNECTOR STATUS is the last stamp that `stamp::Writer`
  gave, also to a frame for which the pool gave no block (`laptop.architect`,
  2026-10-13T02:15:47Z,
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136738770). After a
  refusal as backwards, the refused groups are written again once, with only their
  series, 1 ns after the latest stamp that the refusals give, and a second refusal
  panics (`laptop.architect-2`, 2026-10-13T01:08:55Z, item 7:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6135871024, and
  2026-10-13T02:18:10Z, item 5:
  https://github.com/synnaxlabs/foundation/pull/2518#issuecomment-6136764966, and
  2026-10-20T09:06:36Z, item 2:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6287471425).
  `after` panics for a stamp under the last stamp, since a refusal of the frame of the
  last stamp never gives one. `stamp::Writer`, crate-private, owns the hub's writer
  and the stamps of its frames. Its `write` takes the series of a frame and a fill
  that gets each entry, an index too, with the stamp of the frame, and writes that
  stamp in each index. So `write` writes in no index, and per series it does the work
  of one fill (`laptop.architect-2`, 2026-10-20T11:08:01Z:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6289484340, and
  2026-10-20T11:22:40Z:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6289730092). It gives
  each group of the frame and whether the home applied it, and writes the refused groups
  again. A group for which the pool has no block, or that the home loses or refuses for
  another cause, is not applied. `Expired` and `Codec` panic: a writer with no lease
  never gets `Expired`, and `codec` refuses only raw bytes that are not samples of their
  type, which no fill writes (`laptop.architect`, 2026-10-20T09:09:18Z:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6287519381).
  `connector::poll::Writer` (POLL WRITER) and the status writer each write through it,
  and neither matches the outcomes itself (`laptop.architect-2`, 2026-10-20T09:06:36Z:
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6287471425, on the
  plan https://github.com/synnaxlabs/foundation/issues/2527#issuecomment-6287263795).
  Lost: a `write` that takes only the data series and adds the index of each present
  group, so that only `stamp` knows which entries are indexes. It merges the series of
  each frame, a cost that grows with its series: 3.3 ns for a frame of one series and
  24 ns for 256 on box2. The search of each index grows only with the present groups
  (`laptop.architect-2`, 2026-10-20T10:19:11Z,
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6288704204; the
  reason: `laptop.architect-2`, 2026-10-20T10:44:46Z,
  https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6289116900).
  Lost: a flag of each entry that `write` reads to put the stamp in each index, +159
  ns on `write 256 f64 0`, and a `write` that puts the stamp in each index through
  `Draft::series_mut` after the fill, +172 ns, each the median of 3 runs on a quiet
  host (https://github.com/synnaxlabs/foundation/pull/3556#issuecomment-6289186365):
  the fill already finds each index in its one load.
  The status writer of the supervisor stamps its frames with `stamp::Writer::next`
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
  A kind that subscribes stamps each sample at the mesh time that `Fit` gives for its
  device time, through `poll::Writer::sample`, in place of the midpoint of a request:
  the `opcua` kind unless `polled = true` (OPCUA SUBSCRIBE; `laptop.architect-2`,
  2026-10-19T23:36:41Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6279727836).
- **STAMP FIT (#2622)** `connector::stamp::Fit` fits one device clock to mesh time, and
  `poll::Writer::sample` stamps a sample from the mesh time that a fit gives
  (`laptop.architect-2`, 2026-10-14T01:37:20Z, PR 2:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229). `Fit`
  holds one `estimate::overlap::Overlap` of 200 ppm, over offsets of mesh time minus
  device time, each from `c`, the offset of the first reading of the fit. `mesh` adds
  `c` back. Each sum is in `i128`. `between` gives both edges and `before` gives a high
  edge, in any order of device time. A `between` wider than 2^62 ns gives only its high
  edge. Then a high edge more than 2^62 ns over `c` adds nothing, as the fit already
  holds a high edge under it, and any other edge more than 2^62 ns from `c` starts a new
  fit from its reading, so each edge stays within 2^62 ns plus drift of `c` and no input
  panics (`laptop.architect-2`, 2026-10-19T18:14:00Z, round 3 item 1:
  https://github.com/synnaxlabs/foundation/pull/3386#issuecomment-6274052652). A mesh
  bound past the range of a `Stamp` on the side that claims less becomes `i64::MIN` or
  `i64::MAX` ns, and `mesh` gives `None` past it on the side that claims more
  (`laptop.architect-2`, 2026-10-19T15:07:34Z, B:
  https://github.com/synnaxlabs/foundation/pull/3386#issuecomment-6270475292, which
  supersedes the absolute offsets of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6270224530). A
  property test over any `Monotonic` and mesh time pins it. A reading older than the
  newest is widened by drift to the newest device time in `Overlap` (ESTIMATE FIT, A of
  the same ruling). A `Disjoint` reading restarts the fit from that reading. A clock
  that goes back by less than the width of a reading is not caught until a reading
  disagrees (ESTIMATE FIT). `sample` takes the floor of the midpoint, as `request` does.
  Lost: an edge past the range of a `Span` that stops at that range on the side that
  claims less, and is left out on the other side (B of the same ruling: a first `before`
  far above the clock then leaves each later low edge out, and a stopped high edge can
  hide a disagreement); `c` of 0, which drops each server time past the range of an
  offset; `Fit` inside `stamp::Writer` (a writer with the values of two devices then
  needs two `stamp::Writer` values for one index, so a stamp can go back); a fit that
  keeps each reading and solves a line (it allocates, and its bound is not hard); a
  restart on any edge out of reach (one source time 146 years old drops the bound);
  leaving out a far low edge (a fit from `before` alone never bounds the clock); a
  restart only on a high edge under `i64::MIN`, which keeps the table of B (an old
  reading widened past the range drops an edge; round 3 item 1 of the ruling of
  2026-10-19T18:14:00Z).
