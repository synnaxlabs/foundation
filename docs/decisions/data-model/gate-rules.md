- **GATE RULES (write-path, 2026-10-04)** Writers that do not hold control wait. When
  the holder closes or its control lease runs out, the waiter with the highest
  authority takes control; on a tie, the one that opened first. Each group that the
  home applies or loses renews the control lease of its index, and only that index: a
  live group with no room is lost and renews, and a backfill frame that gets
  `Error::Full` is neither and does not. A writer whose indexes have different rates
  sets its lease by its slowest index. A writer whose control lease ran out stays out
  of the gate until it reopens. Lease and grace times are the home's monotonic time,
  and the X18 grace is a positive span like a control lease. During the grace the
  recorded holder ranks first: the first writer of its subject takes its place, and a
  higher authority takes control. A handoff is recorded only when the holder's subject
  or authority changes. Basis: S11, X18, r8 trace (d). The renewal rule was decided by
  the architect (#1092,
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031035230 and
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031117029).
