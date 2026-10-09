- **SUPERVISOR** `supervisor::Supervisor::run` runs one connector and never starts a
  run before the last one returned, and none after a cancel. Each run gets a child of
  the caller's token, which the supervisor cancels once the run returns or its future
  drops, so each task that the run spawned to wait on it ends with the run
  (`laptop.architect-2`, 2026-10-08T17:58:15Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6065930789). After a
  run returns, the supervisor waits, with no timeout, until each task that the run
  spawned through `Context::tasks` ended, and only then starts its backoff or returns.
  The wait does not count toward the run's length. After a drop of the future of
  `run`, the next `run` of the same name on that supervisor waits for those tasks
  first. A task that does not end at the cancel is a defect of its kind
  (`laptop.architect-2`, 2026-10-08T19:04:20Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6067043337). One
  supervisor runs on each shard, made from `supervisor::Config` (the kinds, clock,
  entropy, network, tasks, and the shard's hub) (`laptop.architect-2`,
  2026-10-08T03:05:58Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6051331538). The
  `hub` field lands after #1941 (`laptop.architect-2`, 2026-10-08T17:58:15Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6065930789, item 4),
  as the tests that need a hub wait on #1941 (`laptop.architect`,
  2026-10-08T17:50:53Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6065807610). After
  `Device` or `Retry` it restarts with full jitter backoff (1 s first, 60 s cap,
  constants). The waits start again from 1 s after a run that lasted at least 60 s.
  `Ok` from `run` ends the connector.
  `Config` returns to the caller, which starts a new supervisor when the spec
  changes (R12-4). Restart errors reach the connector's status in #420. Decided by the
  `connector` builder in the plan on #338, after `/eb-review`; approved by the
  coordinator (#338), with the reset after a long run approved on #338 later.
