- **SUPERVISOR** `supervisor::Supervisor::run` runs one connector and never starts a
  run before the last one returned, and none after a cancel. Each run gets a child of
  the caller's token, which the supervisor cancels once the run returns or its future
  drops, so each task that the run spawned to wait on it ends with the run
  (`laptop.architect-2`, 2026-10-08T17:58:15Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6065930789). A drop of
  the future of `run` cancels the run and does not wait for its tasks. At a spec
  change, `node` cancels each run and awaits it before it starts the new supervisor
  (`laptop.architect-2`, 2026-10-09T01:30:51Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072411231). After a
  run returns, the supervisor waits, with no timeout, until each task that the run
  spawned through `Context::tasks` ended, and only then starts its backoff. A task
  that does not end at the cancel is a defect of its kind (`laptop.architect-2`,
  2026-10-08T19:04:15Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6067043337). It also
  waits so before it returns (`laptop.architect-2`, 2026-10-09T01:06:45Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6072159701, item
  1). The wait does not count toward the run's length (`laptop.architect-2`,
  2026-10-09T01:56:56Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072678811). One
  supervisor runs on each shard, made from `supervisor::Config` (the kinds, clock,
  entropy, network, tasks, and the shard's hub) (`laptop.architect-2`,
  2026-10-08T03:05:58Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6051331538). The
  tests of `connector` and of each kind build that config with
  `connector::testing::create_config`, behind `sim`, which opens the hub through
  `hub::testing::open`. It gives the mesh time as `hub::testing::open` does, and
  drops it when #2143 gives a task mesh time, if no test needs it then
  (`laptop.architect-2`, 2026-10-09T13:37:08Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6082035824;
  `laptop.architect`, 2026-10-08T17:50:53Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6065807610). After
  `Device` or `Retry` it restarts with full jitter backoff (1 s first, 60 s cap,
  constants). The waits start again from 1 s after a run that lasted at least 60 s.
  `Ok` from `run` ends the connector.
  `Config` returns to the caller, which starts a new supervisor when the spec
  changes (R12-4). Restart errors reach the connector's status in #420. Decided by the
  `connector` builder in the plan on #338, after `/eb-review`; approved by the
  coordinator (#338), with the reset after a long run approved on #338 later.
