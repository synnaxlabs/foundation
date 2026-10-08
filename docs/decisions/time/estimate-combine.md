- **ESTIMATE COMBINE (2026-10-04)** A `Measurement` is about one local clock (the node's
  monotonic clock, or a device's sample clock in nanoseconds, #84): its offset is mesh
  time minus the local reading at `at`, and its error is a half-width from 0 to 36500
  days. A bound grows by the drift bound times the time from `at`, in both directions.
  The drift bound is at most 10%; `Drift::UNDISCIPLINED` is 200 ppm. Each source keeps
  its last 8 measurements and offers the one with the smallest bound now. This reads R6
  TIME LOCKED's "keep the fastest exchange" with drift: an old fast exchange loses to a
  fresh slower one. `combine` takes one `Filter` per source and returns the hull of the
  offsets inside more than half of the bounds that vote. A known result holds the true
  offset when more than half of the bounds that vote hold it, whatever the other bounds
  are. It can be wider than the narrowest bound, so C6's "follows the smallest measured
  bound" no longer holds. Cost: PPS at ±100 ns beside two peers at ±1 ms, all centered
  on the true offset, gives ±1 ms, not ±100 ns. Lost: the hull of the offsets inside the
  most bounds (Marzullo), and NTP's selection, which first tries the offsets inside
  every bound. When one lying source of three put a small bound inside the honest
  overlap, each followed the liar. A threshold that also counts the sources with no
  measurement lost too: beside two of them, it needs all three bounds of that case. The
  person decided on 2026-10-05 ("a is fine"), #344. Amends C6 and X36. `combine` fails
  when no offset is inside the bounds of more than half of the sources that vote.
  Decided by the `time` builder (#49). Each source votes: a source with no measurement
  agrees with no offset, and it votes beside the known bounds, or beside the unknown
  bounds when no bound is known. So before its first estimate a clock waits until more
  than half of its sources agree, and one source that answers first cannot set mesh
  time. The person decided on 2026-10-05 ("clock question si approved at whatever path
  you think"), #488. A device's readings
  go to the oscillator fit (`Overlap`), never to `combine`. Node
  sources keep `Filter`, not `Overlap`: a network exchange puts the true offset at about
  the same place in each bracket, so an overlap gains little, and a broken drift bound
  would stay wrong for the life of an overlap, not for 8 exchanges. Decided by the
  coordinator (#84). An error that grows past 36500 days stops at 36500 days ("unknown")
  and never fails, so a lone Windows node gets OS time as OS CLOCK BOUND says, when it
  holds no known estimate (CLOCK HOLDOVER). An error over 36500 days fails only in a new
  measurement: `Measurement::new` gives `None`. The person decided on 2026-10-05 ("Ok
  that's fine"), #225. In an `Interval` from
  `Measurement::interval`, "unknown" is a half-width of 36500 days, and the true time
  can be outside it. Decided by the `time` builder (#142). `combine` uses each bound
  with its full growth, so an "unknown" bound never cuts another. A bound of 36500 days
  at `now`, given or grown by drift, votes only when no bound is known. A vote for it
  lost: it turned a peer split into a wide estimate that no peer gave. The person chose
  this (OS CLOCK BOUND); counting a grown bound is from the `time` builder, approved by
  the coordinator (#314). A known bound votes at any width, so a wide one (a Linux
  bound of 15 s) can still turn a peer split into the hull of both sides. #314
  showed this case before the person chose. When only unknown bounds vote, the estimate
  is unknown too, at the center of the same hull. Approved by the coordinator (#437),
  with the hull of #344. When drift grows unknown bounds so that this hull spans more
  than 73000 days, no unknown estimate holds it, and its center can miss an offset that
  every bound holds. The estimate is then at the center of the offsets inside the most
  bounds. Decided by the `time` builder (#344). `Measurement::unknown(at, offset)` gives
  the "unknown" error, so a source never writes 36500 days itself: 1 ns less is a known
  bound, and it votes until drift grows it to 36500 days. Approved by the coordinator
  (#144). An exchange with an error over 36500 days gives an unknown measurement,
  centered between its edges or at the nearest span, so no caller maps a failure to one.
  It cuts no known bound, because an unknown bound votes only when no bound is known. An
  unknown reading (`exchange::Reading::Unknown`) gives an unknown measurement, because
  two unknown readings sent as intervals whose centers move apart by more than the round
  trip, or one interval clamped at the end of the stamp range, can give a known bound
  (#930). An overlap whose readings allow an error over 36500 days before drift gives
  `None`, as an overlap with no edge does, because no caller needs an unknown device
  measurement yet. A device source can ask for one when it calls `Overlap::at`. Decided
  by the `time` builder (#258), and for the exchange approved by the coordinator (#903).
  Each function returns only the errors it can give: one `Error` per module (`overlap`,
  `combine`), and `Option` where a caller does the same for each cause
  (`Drift::from_ppb`, `Measurement::new`, `Overlap::at`). Decided by the coordinator
  (#272). `Exchange::measure` has one cause left, so it gives `Option` (#903).
