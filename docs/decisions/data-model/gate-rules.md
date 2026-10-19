- **GATE RULES (write-path, 2026-10-04)** Writers that do not hold control wait. When
  the holder closes or its control lease runs out, the waiter with the highest authority
  takes control; on a tie, the one that opened first. Each group that the home applies
  or loses renews the control lease of its index, and only that index: a live group with
  no room is lost and renews, and a backfill frame that gets `Error::Full` is neither
  and does not. A writer whose indexes have different rates sets its lease by its
  slowest index. A writer whose control lease ran out stays out of the gate until it
  reopens. Lease and grace times are the home's monotonic time, and the X18 grace is a
  positive span like a control lease. During the grace the recorded holder ranks first:
  the first writer of its subject takes its place, and a higher authority takes control.
  A handoff is recorded only when the holder's subject, authority, or `via` node
  changes. The gate never compares `via` to choose the holder: a recovered holder
  reopens for its subject through any node (`laptop.architect`, #3104,
  2026-10-19T14:00:04Z, change 2:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946; "to
  choose the holder" and the record when `via` changes: `laptop.architect`, #3379,
  2026-10-19T14:43:11Z:
  https://github.com/synnaxlabs/foundation/pull/3379#issuecomment-6269954055).
  Supersedes "A handoff is recorded only when the holder's subject or authority changes"
  of
  https://github.com/synnaxlabs/foundation/blob/a1704faf5/docs/decisions/data-model/gate-rules.md.
  Basis: S11, X18, r8 trace (d). The renewal rule was decided by the architect (#1092,
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031035230 and
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031117029).
