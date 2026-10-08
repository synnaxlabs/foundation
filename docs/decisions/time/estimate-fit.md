- **ESTIMATE FIT (2026-10-04)** `Overlap` is the oscillator fit for one device clock. It
  keeps the offsets that every reading of that clock allows, each widened by drift, so
  it holds only the reading with the highest low edge and the one with the lowest high
  edge. Readings come in local time order. An older one returns `Backwards` (the device
  restarted), and one that shares no offset returns `Disjoint` (the clock jumped, or it
  drifts faster than its bound). Neither changes the overlap, and the caller starts a
  new one with a gap. Each reading holds true mesh time, with the node's own error in
  its bound, so the drift covers only the device oscillator. The drift is fixed for the
  life of an overlap, because a smaller drift would need readings that it dropped. This
  reads r6 Q5's lower-envelope fit with the rate bounded by `Drift`, not fitted. A line
  fit of offset and rate lost: it is honest only if the rate stays constant, and no
  datasheet bounds oscillator wander. A measured rate needs a signed rate in the model,
  not a smaller `Drift`. A reading gives a low edge, a high edge, or both, at one device
  time. A read return bounds the newest sample the host knows only from above. A low
  edge comes from a mesh stamp before the device acts (a start command or a request),
  from a device counter read between two mesh stamps, or from a latency that the
  hardware guarantees. The overlap gives a bound only when it has both a low edge and a
  high edge (`Overlap::at` gives `None` before that). Decided by the `time` builder; the
  person accepted it on 2026-10-05 ('#1 is fine'). The person accepted one-sided
  readings on 2026-10-05 ("Accept #133"). Supersedes: r6 Q5 method 1 (a fitted rate from
  read-return upper bounds).
