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
  `hub::testing::open`. It gives no mesh time, since a task gets it from its writer
  (#2143) (`laptop.architect-2`, 2026-10-09T13:37:08Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6082035824;
  `laptop.architect`, 2026-10-08T17:50:53Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6065807610). After
  `Device` or `Retry` it restarts with full jitter backoff (1 s first, 60 s cap,
  constants). The waits start again from 1 s after a run that lasted at least 60 s.
  `Ok` from `run` ends the connector.
  `Config` returns to the caller, which starts a new supervisor when the spec
  changes (R12-4). The class of each restart error reaches the connector's status
  (CONNECTOR STATUS), and its text with #420.
  Decided by the `connector` builder in the plan on #338, after `/eb-review`; approved
  by the coordinator (#338), with the reset after a long run approved on #338 later.
- **CONNECTOR STATUS** `Supervisor::run` writes the status channels of its connector,
  `<connector>.status.<name>`, on their own index `<connector>.status.time`, as the
  connector, at `Authority::ABSOLUTE` with no lease (the advisor on #455,
  https://github.com/synnaxlabs/foundation/issues/420#issuecomment-6008006459). The
  index is homed on the connector's node. `connector::status::channels` gives each name
  and sample type:

  | Channel | Type | Values |
  | --- | --- | --- |
  | `state` | `u8` | 0 running, 1 waiting to restart, 2 stopped, 3 ending |
  | `class` | `u8` | the end of the last run: 0 none or `Ok`, 1 `Config`, 2 `Device`, 3 `Retry` |
  | `restarts` | `u64` | the restarts in this call |
  | each count of the kind | `u64` | as the kind sets it through `Context::status` |

  Each write is one frame with the last value of every status channel. Each start of a
  run writes the whole status. A change of `state`, `class`, or `restarts` is written
  at once. A change of counts alone is written at most once each second after the
  last write, timed with the clock of `supervisor::Config` (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1735#issuecomment-6054035730, rules
  1 to 3). When a run returns, `state` 3 with the class of that end is written at
  once. When each task of the run ended, `state` 1, or 2 when the call returns
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/420#issuecomment-6067099903). A
  frame that the home does not apply (`Waiting`, `Reserved`, `Order`, or `Lost`)
  leaves the status staged, so it is written again one second later, also the last
  frame of a call: the call returns once the home applied it, or once `cancel` is
  cancelled. After `Failure::Removed` or `home::Error::Disk` the call writes no more
  status. Each other refusal is a defect of `connector`, and panics. The status writer that does not open panics on an unknown or
  remote channel, which is a defect of `node`, and gives `Ok` when the mesh stopped
  (`laptop.architect-2`, 2026-10-09T18:46:26Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6087144669).
  `status::channels` replaces the public `status::TIME` and `status::CHANNELS`, so one
  call gives each name and type (same ruling). The tests define the status channels
  with `connector::testing::create_status`, behind `sim`, which gives their
  definitions with keys from a `channel::Key` on. `sim` also turns on the optional
  dependency on `spec`, whose `Definition` the helper gives.
  `connector::status::channels` panics on a status name over `Name::MAX_BYTES`: the
  counts come from a kind's code, so the name is internal. Lost: a helper that calls
  `Hub::set_definitions` itself, because each call replaces all definitions and a
  test sets its own in the same call (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6087430425).
