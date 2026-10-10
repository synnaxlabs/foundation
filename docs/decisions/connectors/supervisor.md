- **SUPERVISOR** `supervisor::Supervisor::run` runs one connector and never starts a
  run before the last one returned, and none after a cancel. Each run gets a child of
  the caller's token, which the supervisor cancels once the run returns or its future
  drops, so each task that the run spawned to wait on it ends with the run
  (`laptop.architect-2`, 2026-10-08T17:58:15Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6065930789). A drop of
  the future of `run` cancels the run and does not wait for its tasks
  (`laptop.architect-2`, 2026-10-09T01:30:51Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072411231). At a
  spec change, `node` cancels the run of each connector that the change removes or
  changes, and starts the new run of a changed connector only after its old `run`
  returned. Each other connector keeps its run, and the shard keeps its supervisor
  (`laptop.architect-2`, 2026-10-09T05:10:43Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6074718616).
  Supersedes "At a spec change, `node` cancels each run and awaits it before it starts
  the new supervisor" of
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072411231. A
  connector that a change adds starts at once, unless the last run of its name has
  not returned: then it starts after that run returned, so no two runs write one set
  of status channels. A run that a change cancels before it starts ends at once, so a
  name has at most one run that waits. This departs from "A connector that the change
  adds: start its run at once" of 6074718616, item 3, and waits on the ruling of item 2
  of https://github.com/synnaxlabs/foundation/pull/2261#issuecomment-6098749526. After a
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
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6051331538). Only
  a shard that has a hub runs one, so today only shard 0 (`laptop.architect`,
  2026-10-10T14:41:53Z:
  https://github.com/synnaxlabs/foundation/pull/2261#issuecomment-6098661378; it waits
  on the ruling of `laptop.architect-2` on item 1 of 6098749526). The
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
  `Config` returns to the caller, which calls `run` again when the spec changes the
  connector (R12-4) (`laptop.architect-2`, 2026-10-09T05:10:43Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6074718616).
  Supersedes "The caller (`node`) already watches the spec and starts a new supervisor
  on a change (R12-4)" of the plan on #338
  (https://github.com/synnaxlabs/foundation/issues/338#issuecomment-5994872059). The
  class of each restart error reaches the connector's status (CONNECTOR STATUS), and
  its text with #420.
  Decided by the `connector` builder in the plan on #338, after `/eb-review`; approved
  by the coordinator (#338), with the reset after a long run approved on #338 later.
- **CONNECTOR STATUS** `Supervisor::run` writes the status channels of its connector,
  `<connector>.status.<name>`, on their own index `<connector>.status.time`, as the
  connector, at `Authority::ABSOLUTE` (the advisor on #455, 2026-10-06T02:20:11Z:
  https://github.com/synnaxlabs/foundation/issues/420#issuecomment-6008006459), with
  no lease (`laptop.architect-2`, 2026-10-09T18:46:26Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6087144669). The
  index is homed on the connector's node. `connector::status::channels` gives each name
  and sample type:

  | Channel | Type | Values |
  | --- | --- | --- |
  | `state` | `u8` | 0 running, 1 waiting to restart, 2 stopped, 3 ending |
  | `class` | `u8` | the end of the last run: 0 none or `Ok`, 1 `Config`, 2 `Device`, 3 `Retry` |
  | `restarts` | `u64` | the restarts in this call |
  | each count of the kind | `u64` | as the kind sets it through `Context::count` |

  Each write is one frame with the last value of every status channel. Each start of a
  run writes the whole status. A change of `state`, `class`, or `restarts` is written as
  soon as the home applied the state before it (below). A change of counts alone is
  written at most once each second after the last write, timed with the clock of
  `supervisor::Config` (`laptop.architect-2`, 2026-10-08T06:38:42Z:
  https://github.com/synnaxlabs/foundation/issues/1735#issuecomment-6054035730, rules 1
  to 3). When a run returns, `state` 3 with the class of that end is written as soon as
  the home applied the state before it. When each task of the run ended, `state` 1, or 2
  when the call returns
  (`laptop.architect-2`, 2026-10-08T19:07:43Z:
  https://github.com/synnaxlabs/foundation/issues/420#issuecomment-6067099903). A run
  whose last task ends T after its start gives at most 1 + ⌊T / 1 s⌋ + 2 frames: the
  start, at most one frame of counts in each second, then `state` 3 and the next state.
  So a kind that counts 10,000 samples over 10 s gives at most 13. When the run itself
  sets its last count and ends at the flush at 10 s, that flush writes no frame, and it
  gives 12. T runs to the end of the last task, because a task that outlives the run
  can set counts between `state` 3 and the next state (`laptop.architect-2`,
  2026-10-09T21:16:18Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6089396653).
  Supersedes the bound of 12 frames of
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088160535.
  `state` 3 and the next state stay two frames also when no task is left, because
  `state` 3 marks the end of the run (`laptop.architect-2`, 2026-10-09T19:51:38Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088160535).
  `status::Writer` holds the rules of `state`, `class`, and `restarts`, and the
  supervisor calls its `start`, `end`, `wait`, and `stop` (same ruling). A frame that
  the home does not apply (`Waiting`, `Reserved`, `Order`, or `Lost`), or for which the
  shard's pool has no block now (`block::Error::Exhausted` or `Refused` in
  `frame::Error::Pool`), leaves the status staged, so it is written again one second
  later, also the last frame of a call: the call returns once the home applied it, or
  once `cancel` is cancelled. The pool case is load, not a defect
  (`laptop.architect-2`, 2026-10-09T21:32:53Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6089614613). A
  status frame larger than the largest block of the pool (`block::Error::TooLarge`) is
  a defect, and panics (`laptop.architect-2`, 2026-10-09T21:40:46Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6089720287). A
  change of state waits until the home applied the state before it, or until `cancel`
  is cancelled, so no state replaces a state that the home did not apply. A start
  whose wait a cancel ends writes nothing, and no run starts. After
  `Failure::Removed` or `home::Error::Disk`, no change of state waits
  (`laptop.architect-2`, 2026-10-09T21:46:07Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6089790301).
  Supersedes "at once" in rule 3 of
  https://github.com/synnaxlabs/foundation/issues/1735#issuecomment-6054035730 and
  "The frame's time is when the wait started" in
  https://github.com/synnaxlabs/foundation/issues/420#issuecomment-6067099903: the
  time of a frame is the time of the hub at its write, or the nanosecond after the
  last stamp of the writer when that is later (`laptop.architect`,
  2026-10-09T23:05:03Z:
  https://github.com/synnaxlabs/foundation/pull/2173#issuecomment-6090762715). After
  `Failure::Removed` or `home::Error::Disk` the call writes no more status. Each other
  refusal is a defect of `connector`, and panics (`laptop.architect-2`,
  2026-10-09T19:41:05Z:
  https://github.com/synnaxlabs/foundation/pull/2173#issuecomment-6087998186). While
  the shard's pool gives no frame, a change of state waits also after its status
  channels were removed or its disk failed, until the pool frees or `cancel` is
  cancelled (`laptop.architect-2`, 2026-10-09T23:16:59Z and 2026-10-09T23:33:09Z:
  https://github.com/synnaxlabs/foundation/pull/2173#issuecomment-6090887547 and
  https://github.com/synnaxlabs/foundation/pull/2173#issuecomment-6091053596). A frame
  that the home refuses as `Backwards` is stamped again after the stamp that the
  refusal gives, and written again at once, one time. A writer starts with no last
  stamp, so its first frame can be at or before the last frame of the writer before it
  on the same index (`laptop.architect-2`, 2026-10-09T20:20:46Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088599351). Each
  other refusal of the frame written again follows the rules above. A second
  `Backwards` is a defect, and panics (`laptop.architect-2`, 2026-10-09T20:24:02Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088648651). The
  status channels open once the node has mesh time, and no run starts before. A cancel
  while they wait to open gives `Ok` at once (`laptop.architect-2`,
  2026-10-09T19:43:41Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088037219, item 4).
  The tests of that wait open the hub with `hub::testing::open_unsynced`, whose mesh
  time starts after a delay (`laptop.architect`, 2026-10-09T19:41:28Z:
  https://github.com/synnaxlabs/foundation/pull/2173#issuecomment-6088003915). The
  status writer that does not open panics on an unknown or remote channel, which is a
  defect of `node`, and gives `Ok` when the mesh stopped (`laptop.architect-2`,
  2026-10-09T18:46:26Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6087144669). An
  unknown channel after the cancel of the call also gives `Ok`, since `node` removes
  the status channels of a connector before it cancels its run. This departs from item
  2 of 6087144669, and waits on the ruling of item 4 of
  https://github.com/synnaxlabs/foundation/pull/2261#issuecomment-6098749526.
  `status::channels` replaces the public `status::TIME` and `status::CHANNELS`, so one
  call gives each name and type (same ruling). The tests define the status channels with
  `connector::testing::create_status`, behind `sim`, which gives their definitions with
  keys from a `channel::Key` on. `sim` also turns on the optional dependency on `spec`,
  whose `Definition` the helper gives. Lost: a helper that calls `Hub::set_definitions`
  itself, because each call replaces all definitions and a test sets its own in the same
  call (`laptop.architect-2`, 2026-10-09T19:04:30Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6087430425).
  `connector::status::channels` gives `types::name::Error::Long` for the first status
  name over `Name::MAX_BYTES`, because the connector's name comes from the user's file.
  `config::plan` gives that error as a diagnostic at the connector's label (#1821).
  `Writer::open` panics on it, because the plan refused the name. Lost: a public
  constant of the longest status suffix. The count rules stay in `kind::Table::check`,
  which has the kind's name for the message, and `channels` takes `counts` as `check`
  gives them. A kind gets a count through `Context::count`, and `status::Status` stays
  inside the crate (`laptop.architect-2`, 2026-10-09T19:43:41Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088037219, items 1
  to 3).
