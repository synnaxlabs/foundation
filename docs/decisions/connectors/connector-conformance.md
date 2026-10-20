- **CONNECTOR CONFORMANCE (#2247, #2641)** `oracles/conformance/connector/` holds the
  scenarios that each connector kind passes, the oracle of D4's conformance kit. Each
  kind crate runs them as its `conformance` test, from `<kind>/main.rs`: a `Suite` and
  one `suite!` line, which expands to one `#[test]` for each scenario, and two that
  check that a scenario that is not done at `RING` fails. A `<kind>/main.rs` can also
  hold a test of that kind alone, which uses `scenarios::run` and the `pub(crate)` items
  of `scenarios.rs` (`opcua`: a config with no `read` block; `laptop.architect-2`,
  2026-10-18T22:00:39Z:
  https://github.com/synnaxlabs/foundation/pull/3272#issuecomment-6255674282). The
  `[[test]]` entry of each kind is part of the oracle. The scenarios use only public
  items: the rig of `connector::testing`, `hub::testing::Env`, the sim, `env`,
  `document`, `types`, `create_secret` of `connector::testing`, and the `kind`,
  `status`, `reader`, and `cancel` modules of `connector`.
  Each runs two sim nodes: the plant (the rig and the connector) and the device (the
  kind's simulator). A `Suite` gives plain inputs: its table, a setup that defines what
  the connector needs and starts each writer that feeds it, its simulator, the name of
  its progress count, its backoff form, the configs that its check refuses with
  their exact diagnostics, a function that makes its config read each secret that it
  takes, and an echo simulator when its device can answer with a text of its own
  (both approved by the same comment as the next sentence). The
  kind makes the marked config, as it owns the shape of its config: a secret can need
  other attributes to check (`laptop.architect-2`, 2026-10-20T14:37:34Z:
  https://github.com/synnaxlabs/foundation/issues/2642#issuecomment-6293427967). The
  simulator of a kind that subscribes changes its data at least every 100 ms, so its
  progress grows (item 1 of the same comment): the simulator of `opcua` changes each
  variable every 100 ms, so the suite runs the default mode, which subscribes.
  `polled = true` keeps its crate and ONE NODE tests (`laptop.architect-2`,
  2026-10-20T10:03:28Z:
  https://github.com/synnaxlabs/foundation/pull/3548#issuecomment-6288442532). People
  own the scenarios. A kind that fails one has a defect, or the person changes the
  scenario. The scenarios cover kinds whose device is a TCP address: a device that is
  not TCP waits on #3075, until the first suite of a kind with no TCP carrier.

  "Heals" means: within `HEAL` (the backoff cap of 1 min, plus 10 s), the state is
  Running, the error is gone, and the progress count grows past its value at the start
  of the heal. The backoff form is `Restart` (the run ends, and the supervisor shows
  Restarting with restarts that grow) or `Run` (the run goes on: Running, class Retry, 0
  restarts, and no progress, as INFLUX KIND locks for `influx`). Scenarios 2, 3, and 4
  check the form that the kind names, and refuse the other one. One status state for a
  run that waits between tries waits on #3076, until the next change of `status::State`
  or a third form. The rulings of these two deferrals and of the form of scenario 5:
  `laptop.architect-2`, 2026-10-17T22:04:56Z
  (https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241915192).
  `laptop.monitor` accepted both deferrals with one condition (relayed by
  `laptop.architect-2`, 2026-10-17T22:06:42Z): each form names the cause and the address
  of the device, which scenarios 2 and 3 check
  (https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241940271).

  A scenario that runs past 90 s of plant time (`RING`, the sum of the steps of its sim)
  fails. The 4 MiB ring of `home::testing::shard` fills after about 100 s for `opcua`
  and for `influx` (finding 7 of
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241787008), and then
  can lose a status, so a check of an absence can pass. The longest of scenarios 1 to 5
  runs 20.2 s of plant time (`influx`, the device does not answer), and scenario 2 of
  `influx` runs 19.7 s (box1, at 2026-10-17). A later suite measures its own fill. The
  longest of scenarios 1 to 5 of `modbus` runs 6.1 s (box1, at 2026-10-18). The longest
  run of scenario 6 takes 50.2 s for `modbus` (seed 0x138), 49.7 s for `opcua` (seed
  0xbf), and 45.9 s for `influx` (seed 0x370) (box1, at 2026-10-20). Over seeds 0 to
  99999 of `modbus` and 0 to 49999 of `influx` and `opcua`, it takes 76.1 s (`modbus`,
  seed 0x11802) (box1, at 2026-10-20). The trigger is the merge of #3050, which trims
  the ring and adds the two tests that fail now
  (https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6241900155). #3050
  removes `RING`, its check in `run`, and the `suite!` tests `fails_past_the_ring` and
  `fails_50_ms_past_the_ring`, with the person's approval, as they are in `oracles/`. OK
  of `laptop.architect`, 2026-10-17T21:54:10Z:
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241806249. The form
  of the check: `laptop.architect-2`, 2026-10-17T22:04:56Z:
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241915192.

  1. **Starts.** Within 10 s: Running, no error, 0 restarts, and progress above 0.
  2. **Device absent at start.** No simulator. Within 1 min, the backoff form, an
     error text that names the address of the device, never class Config.
     `Restart`: Restarting, class Device or Retry, and restarts that grow to 2.
     `Run`: Running, class Retry, 0 restarts, and progress 0, and no status of the
     next 10 s leaves it. A status writes only on a change, so this checks each
     change in the 10 s, and none when nothing changes. Then the simulator starts:
     heals. In a second form, the device does not answer: the simulator serves
     behind a link that delays each packet by 1 h, and within 1 min comes the same
     backoff form, with no heal. Ruling of `laptop.architect-2` at 2026-10-17T22:04:56Z
     (https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241915192).
  3. **Device lost while running**, in two forms: the simulator stops (each
     connection closed), and the device node crashes (`Sim::crash`, no close reaches
     the plant). Within 30 s: an error of class Device or Retry whose text names the
     address of the device, in the backoff form (`Run`: Running and 0 restarts).
     Then the device comes back: heals, no count and no restart count went down, and
     for `Restart`, the restarts grew.
  4. **Stop.** `Run::stop` gives `Ok` within 5 s while Running, in the backoff form,
     and while the plant connects over a link that delays each packet by 1 h (the
     stop comes at the first millisecond that the plant holds 1 stream, within 1 s,
     so the stop falls inside the connect). After it, the plant node holds no open
     stream and no task of the run (`Run::stop` gives its result once each task of
     the run ended). The connect form with a link that loses each packet (loss 1)
     comes back with #3040. The form of the stop in a connect: `laptop.architect-2`,
     2026-10-18T02:21:42Z
     (https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6244674229).
  5. **Config.** For each refused config, `Table::check` gives exactly its
     diagnostics, and `Rig::supervise` ends with the same `Error::Config`. For a kind
     whose check gives writes, a write channel that no spec defines gives
     Stopped, class Config, 0 restarts, and the text of `connector.unknown-channel`.
     For a kind whose check gives a reader, a reader whose selector matches only a
     name that no spec defines gives Stopped, class Config, 0 restarts, and the text
     of `connector.no-channel`. A kind with no writes or no reader passes that form
     with no check.

  6. **Random faults.** Over seeds 0 to 999 on each merge, each a run of its own: the
     connector runs, then meets 1 to 8 faults at random times in 15 s, each one of:
     new links to and from the device (a delay below 2 s, a jitter below 500 ms, and a
     loss of 0 to 1 in quarters), a stop of the simulator, a crash of the device node,
     or a start of the simulator. The seed draws the faults and seeds the sim. While
     they come: no panic, never class Config, each error text at most 1024 bytes, and
     no count and no restart count goes down. After the last fault, both links are as
     at the start and the simulator serves: heals. `CONFORMANCE_SEED` gives other
     seeds in hex: one seed, or a range `<start>..<end>`, the end excluded, such as
     `0x0..0xf4240` for a nightly run. A failed run panics with its replay command,
     which sets `CONFORMANCE_SEED` to its seed.

     The plan's duplication is not drawn: a TCP stack hides a duplicate segment from
     the application, so no kind of this suite can see one. It comes back with the
     first suite of a kind that is not TCP (#3075). `connector::status` cuts each text
     to 1024 bytes, so the check of the bound guards that cut, and no kind can fail it
     alone. No run of the three kinds gives an error text near the bound (112 bytes at
     most, box1, at 2026-10-20). Until #3553, a seed whose links fill the backlog of
     the device fails with the panic of the sim at a full backlog, not for a defect of
     the connector. #3554 adds the nightly range after #3553, and the replay of a
     failed seed in CI. The numbers of this scenario, and the duplication that it does
     not draw: approved by `laptop.architect-2`, 2026-10-20T08:40:04Z
     (https://github.com/synnaxlabs/foundation/pull/3548#issuecomment-6286993255).

  7. **No secret in a status.** For a kind whose config takes secrets, the config of its
     setup reads each of them from a secret whose value is a marker, and checks. No
     status, no end of a run, and no check shows the marker. One run meets the
     simulator, and is Running with progress within 10 s, then its stop, and gives an
     error within 30 s and 5 s more. A second run meets no device: an error within 10 s,
     and 5 s more. A third meets the echo simulator, which answers each request with an
     error whose text holds each header of the request, as a gateway in debug mode
     does: an error within 10 s, and 5 s more. Then the write form of scenario 5, with
     the same secrets, for 5 s. A refused
     config and the read form stop before a run reads a secret, so they get no check.
     A kind with no secret passes with no check.
     The status hides each secret value that a run read (ruling of `laptop.architect-2`
     on #2849 item 4, 2026-10-16T17:54:49Z:
     https://github.com/synnaxlabs/foundation/issues/2849#issuecomment-6221505945).
     Scenario 7 runs 13.1 s of plant time for `influx` with no echo simulator (box1,
     at 2026-10-20).

  The numbers are tunable by the architect of `connector`. Plan of
  `laptop.architect-2` at 2026-10-13T21:22:43Z
  (https://github.com/synnaxlabs/foundation/issues/2247#issuecomment-6155477394), with
  the fixes of 2026-10-17T16:05:31Z
  (https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6236306639, item
  5 for the delay of 1 h). The backoff form changes the plan, which asked for
  Restarting in scenarios 2 and 4: proposed by `box1.builder-2` at
  2026-10-17T21:08:01Z
  (https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6241104680), no
  objection of `laptop.architect-2` at 2026-10-17T21:10:51Z
  (https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6241157217). The
  form sits in the suite file, so a kind that changes its form changes its oracle.
  Lost: one check that takes either form, which hides that change; the scenarios in a
  module of `connector`, where an agent could change an oracle in a normal PR; and
  scenarios that each kind writes with common assertion helpers, where nothing makes a
  kind run each scenario.
