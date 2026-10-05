# R18: How the Synnax Driver timestamps hardware-timed samples

Scope: `driver/` at `11d47c9d6b` (working tree), plus `x/cpp` helpers it calls. Paths
starting with `driver/`, `x/`, `client/`, or `site/` are relative to the repo root; all
other paths are under `driver/`. Bare `:N` cites the file named last. Bare
`LabJackM.h` and `LabJackMModbusMap.h` are in `labjack/ljm/`; bare `nidaqmx.h`,
`sugared.h`, and `api.h` are in `ni/daqmx/`. "Derived" marks my own analysis, not a fact the code or a
test states. "Vendor docs" marks a device fact I did not verify in this repo.

## Summary

- The class is a full PID (`driver/common/sample_clock.h:79-108`, `:181-216`). The
  deployed clock is P-only: the one production constructor sets Kp = 0.01, Ki = Kd = 0
  (`:94`, `:118-132`), and no config path reaches Ki, Kd, the integral cap, or the
  back-correction factor. The only user knob is `correct_skew` on/off (`:38-53`). The
  earlier study is right about behavior; the person is right about the class.
- It is a first-order low-pass of read-return times along a nominal-rate line. Stamps
  carry the mean read latency as bias, and nothing bounds them. They can lead host time:
  a test pins a block end 0.4 s ahead of "now" (`sample_clock_test.cpp:184-211`).
- The time source is `std::chrono::system_clock` (`x/cpp/telem/telem.h:398-408`), so OS
  clock steps enter every stamp.
- The Driver already reads a device sample counter on every NI analog read
  (`driver/ni/hardware/daqmx.cpp:136-151`) and LJM backlogs on every LabJack read
  (`driver/labjack/read_task.h:729-745`). It uses both only for a warning.
- No driver reads a device timestamp. DAQmx first-sample timestamps, LabJack
  `STREAM_START_TIME_STAMP` and `CORE_TIMER`, EtherCAT distributed clocks, and OPC UA
  source timestamps are all reachable and unused.
- Gaps are never detected from timing. Recovery resets the clock to a new host anchor.
- History: four timing rewrites in two weeks in 2025-03 (SY-2200, SY-2240, SY-2245,
  SY-2247), then SY-3310 (future-dated stamps killed a restarted task) and SY-4693
  (software timer lost up to 44.6% of its rate).

## 1. What the sample clock does

### Interface

`SampleClock` has `reset()`, `wait(breaker)` (returns the first-sample stamp of the
next block) and `end()` (returns the block end) (`sample_clock.h:22-35`). Read tasks
stamp a block with `generate_index_data`, an end-exclusive linspace of `n` stamps over
`[start, end)` (`sample_clock.h:219-241`, `x/cpp/telem/series.h:1185-1209`). Every
index channel of the task gets the same series (`sample_clock.h:235-240`).

### Software-timed clock

`wait` takes `now()` before it sleeps on an `x::loop::Timer`, and returns that pre-sleep
stamp; `end` returns `now()` (`sample_clock.h:65-71`). The block spans [pre-sleep,
post-read].

### Hardware-timed clock: the algorithm

- First `wait` after a reset anchors the block start and `prev_system_end` to `now()`
  (`:172-178`). Later `wait` calls return the previous corrected end.
- `end()` (`:181-216`):
  - `sample_end = start + sample_rate.period() * samples_per_chan` (`:188-190`). It uses
    sample count times sample period, not the stream period, so 2.5 kHz at 200 Hz
    (12.5 per read) stays correct (`:182-187`). `samples_per_chan` is a truncated
    `size_t` (`x/cpp/telem/telem.h:696`, `driver/ni/read_task.h:81`).
  - `error = sample_end - now()` in ns (`:191-194`); `dt` = host time since the last
    `end` (`:195-197`).
  - `p = Kp*error`; `integral += error*dt`, clamped to
    `±max_integral * stream_period_ns` (`:198-204`, `:114-116`); `i = Ki*integral`;
    `d = Kd*(error - prev_error)/dt` (`:205-206`).
  - `correction = p + i + d`, capped only above at
    `max_back_correction_factor * stream_period` (`:208-211`, `:110-112`). Forward
    correction (host ahead of the line) has no cap.
  - `end = sample_end - correction` becomes the next block start (`:212-215`).

### Parameters and defaults

| Field | Default | Units, notes |
| --- | --- | --- |
| `now` | `TimeStamp::now` | system realtime clock (`:76`) |
| `k_p` | 0.01 | unitless (`:90`, `:94`) |
| `k_i` | 0 | doc says 1/ns (`:91`, `:94`) |
| `k_d` | 0 | doc says ns (`:92`, `:94`) |
| `max_integral` | 0.1 | fraction of stream period in ns (`:99`, `:114-116`) |
| `max_back_correction_factor` | 0.5 | fraction of stream period (`:108`) |

Notes on the integral: the doc says the cap defaults to "1x the stream period" but the
value is 0.1 (`:95-99`). The integral holds ns² while the clamp is in ns, so it
saturates within one cycle and `i` acts as a sign term of at most
`Ki * 0.1 * period_ns` (derived). With Ki = 0 in production this has no effect.

Production construction: NI (`driver/ni/read_task.h:240-252`) and LabJack
(`driver/labjack/read_task.h:649-655`) call `create_simple(sample_rate, stream_rate,
correct_skew)`, which keeps the defaults or zeros all three gains when `correct_skew`
is false (`sample_clock.h:118-132`). `correct_skew` defaults to true (`:41`) and comes
from the rack config file, env, or CLI (`driver/rack/file.cpp:32-33`,
`driver/rack/env.cpp:20`, `driver/rack/args.cpp:15`). No other reference to `k_i`,
`k_d`, `max_integral`, or `max_back_correction_factor` exists outside the header and its
test.

### Time source

- Stamps: `TimeStamp::now()` is `system_clock` (`x/cpp/telem/telem.h:398-408`); its own
  comment flags the realtime/steady confusion (`:399-401`).
- Pacing (software clock, EtherCAT engine): `x::loop::Timer` uses
  `high_resolution_clock` (`x/cpp/loop/loop.h:20`, `:35-39`). That is `system_clock` on
  libstdc++ and `steady_clock` on libc++ and MSVC (standard library fact).
- The timer anchors each period to its deadline, not the wake time (`loop.h:63-65`,
  `:84`), since SY-4693.

### How it starts and restarts

- NI: `start()` resets the clock, then `StartTask` (`driver/ni/read_task.h:296-308`).
  The anchor is the `now()` at the first `wait` in the first `read` (`:341`), after the
  device started.
- LabJack: `restart()` calls `eStreamStart`, then resets (`labjack/read_task.h:697-704`).
- NI in-loop recovery: a REQUIRES_RESTART error restarts the task and the clock
  (`ni/read_task.h:354-358`). The first good read after any read error resets the clock
  and drops that read's samples (`:361-364`).
- The writer opens at the first stamp of the first frame
  (`driver/pipeline/acquisition.cpp:85-96`, `:147-151`).

### What the tests pin (`driver/common/sample_clock_test.cpp`)

- Software clock: start ≥ the `now()` before `wait`, end ≥ the `now()` before `end`
  (`:20-29`).
- Kp = 0: stamps sit exactly on the nominal line, and each start equals the previous
  end (`:32-58`, `:154-181`).
- Forward correction is uncapped: Kp = 0.1, host 250 ms late, end = 2 s + 25 ms
  (`:61-90`).
- Reset re-anchors to `now()` (`:93-121`).
- Back-correction cap: Kp = 2, factor 0.1, host at 0.5 s, end = 0.9 s (`:184-211`). The
  stamp leads host time by 0.4 s.
- Convergence: six parameter sets, all with non-production gains (`:309-409`). They
  assert a steady-state mean per-cycle period error below 5% of the injected offset and
  a max below 5% of the stream period (`:299-305`). They measure period error (`:275-280`),
  never the absolute offset from a true sample time.
- No test uses `create_simple` or the production gains (grep of `*_test.cpp`).

### Derived behavior

Let `x_k = end_k - h_k`, with `h_k` the host time at read return, `N` samples per read,
and `d = N(T_nominal - T_device)`. Uncapped, `x_k = (1-Kp)(x_{k-1} + N*T_nominal -
Δh_k)`, so in steady state `x = d(1-Kp)/Kp ≈ 99d`. At 100 ppm and a 0.1 s read period
that is about 1 ms, ahead of `h` when the device runs fast. A one-off host delay δ moves
a stamp by Kp·δ (1%). The time constant is 100 reads (10 s at a 10 Hz stream rate).
Since `h_k` = true time + transfer latency + backlog, stamps carry that sum as bias. With
`correct_skew` off, stamps are open loop from the first anchor and drift without bound.

## 2. What the clock handles that ESTIMATE FIT does not, or handles differently

- Monotonic output. The back-correction cap keeps a block from starting before the last
  one (`sample_clock.h:100-108`). ESTIMATE FIT gives a feasible band; it still needs a
  rule that the chosen stamp never decreases, across fits too.
- Non-integer samples per stream period: the increment counts samples, not stream
  periods (`:182-189`).
- Device restart: no detection in the clock. NI logs when `TotalSampPerChanAcquired`
  goes backwards and resets the request count (`ni/hardware/daqmx.cpp:143-146`), but the
  clock keeps its line. Restart paths reset it to a new host anchor
  (`ni/read_task.h:302`, `:354-364`; `labjack/read_task.h:704`, `:737`).
- Buffer overrun and lost samples: NI sets `DAQmx_Val_OverwriteUnreadSamps`
  (`ni/hardware/daqmx.cpp:127-130`). Whether the next read then skips or fails with
  -200279 (`ni/daqmx/nidaqmx.h:9160`) is DAQmx behavior not visible here; the clock gets
  no gap signal either way. LJM auto-recovery inserts `-9999` dummy scans so the scan
  count stays true (`labjack/ljm/LabJackM.h:187`, `:1806-1810`); the Driver writes them
  as data. The P loop absorbs any index jump at 1% per read. ESTIMATE FIT breaks the fit
  instead, which is strictly better.
- "Skew" in Synnax means backlog (acquired minus requested samples), not clock skew
  (`ni/hardware/daqmx.cpp:149-150`). The warning fires at 1 s of samples on NI
  (`ni/read_task.h:95-98`), 2 s device and 1 s LJM backlog on LabJack
  (`labjack/read_task.h:416-427`). The text tells the user to lower the stream rate
  (`common/read_task.h:249-254`).
- USB stalls and late reads: the late read moves the stamp forward by 1% of the delay,
  and the burst of fast reads after it hits the back cap. Stamps stay on the device line
  (derived). The upper bound from read-return is loose by the whole backlog.
- Rate changes: rates are fixed per task config; a change rebuilds the task. LabJack's
  actual scan rate from `eStreamStart` is discarded (`labjack/read_task.h:696-704`,
  `LabJackM.h:941-943`); NI never reads back a coerced rate (no `SampClk_Rate` call
  outside `ni/daqmx/`). The P loop absorbs the nominal-rate error as a steady offset.
- Warm-up: none beyond the 100-read time constant. The first anchor is after device
  start, so early stamps are late by the start latency (derived).
- Host clock steps: forward steps are slewed at 1% per read; backward steps at most half
  a stream period per read (derived from `:208-211`). Software-timed tasks stamp raw
  `now()` and take steps directly; PR #2843 lists a backward step as an open risk.
- Task restart: a fresh anchor; the pipeline also opens a new writer at a fresh start
  (`pipeline/acquisition_test.cpp:970-1000`). Overlap with the last run is not checked
  in the Driver; the Core rejects it (SY-3310).
- Samples per read: fixed per task; NI reads exactly that many with an infinite wait
  (`ni/hardware/daqmx.cpp:107-116`). A short read would be mis-stamped.
- Skew between channels: all channels in one task share one series. Multiplexed
  converters (sequential channels within a scan) are not modeled. Between tasks or
  devices, alignment is only as good as each task's host anchor; the docs state that
  goal: hardware-timed samples "stay in line with the computer clock and with software
  timed tasks on the same Driver"
  (`site/docs/src/pages/reference/hardware/tasks.mdx:188-192`).
- Driver host versus Core: a midpoint ping measures offset and warns above 1 s
  (`x/cpp/telem/clock_skew.h:37-49`, `client/cpp/synnax.h:60`,
  `client/cpp/connection/checker.cpp:106-130`). Nothing corrects stamps with it.

## 3. Device counters and timestamps by driver

| Driver | Source | Read today | During stream | Bound it can give |
| --- | --- | --- | --- | --- |
| NI analog, counter | `TotalSampPerChanAcquired` | yes, after each read (`ni/hardware/daqmx.cpp:136-151`) | yes | upper bound on the newest index at the next host stamp; lower bound only with a max transfer latency |
| NI digital | none | no backlog read (`daqmx.cpp:77-95`) | n/a | read-return only |
| NI | `AvailSampPerChan`, `CurrReadPos` (`nidaqmx.h:912-913`) | no | yes | same as above |
| NI | `FirstSampTimestamp_Val`, `FirstSampClk_When`, `StartTrig_TimestampVal`, `SyncPulse_Time_When` (`nidaqmx.h:1075`, `:1084-1089`, `:1131-1133`) | wrapped (`ni/daqmx/api.h:1354`, `sugared.h:1347`), never called | one value per run | device time of sample 0; time-based sync hardware only (vendor docs) |
| NI | `RealTime_ReportMissedSamp`, `WaitForNextSampleClock(isLate)` (`nidaqmx.h:950-953`, `ni/daqmx/prod.cpp:9758`) | no | yes | lateness flag, a gap signal |
| LabJack stream | device and LJM scan backlog | yes, each `eStreamRead` (`labjack/read_task.h:729-745`) | yes | tighter upper bound for returned scans |
| LabJack stream | actual scan rate from `eStreamStart` | returned, discarded (`:696-704`) | at start | true nominal rate |
| LabJack stream | `STREAM_START_TIME_STAMP` (`LabJackMModbusMap.h:1344-1346`) | no | after start | `CORE_TIMER` at the first scan |
| LabJack | `CORE_TIMER` (`LabJackMModbusMap.h:17920-17922`), `STREAM_DATA_CAPTURE_16` (`:2060-2061`) | no | readable, and streamable per scan (vendor docs) | two-sided device-to-host bracket from a stamped register read |
| LabJack stream | `-9999` dummy scans (`LabJackM.h:1806-1810`) | written as data | yes | exact count of lost scans |
| LabJack unary | `skipped_intervals` from LJM interval | ignored (`labjack/read_task.h:572-578`) | yes | missed-period count |
| EtherCAT | distributed clocks (`ethercat/igh/ecrt.h:783-829`, `:967-981`) | not configured, not read; SOEM wrapper has no DC calls | each cycle | slave time of input latch |
| EtherCAT | send and receive host times | not stamped (`ethercat/engine/engine.cpp:44-85`) | each cycle | two-sided bracket of the input latch |
| OPC UA | `sourceTimestamp`, `serverTimestamp` in each `DataValue` | never read; request leaves `timestampsToReturn` at 0, which is Source per the OPC UA enum (`opcua/types/types.h:259-265`) | yes | server-side sample time |
| OPC UA unary | request bracket | midpoint stamp (`opcua/read_task.h:341`, `:377-380`) | yes | two-sided bound on service time, not on sampling |
| Modbus | none in protocol | midpoint of [pre-sleep, post-read] (`modbus/read_task.h:389-397`) | n/a | see note |
| HTTP | request bracket or a JSON time field | midpoint (`http/read_task.cpp:364-372`, `http/processor/processor.cpp:82`, `:159`) or field (`http/read_task.cpp:51-56`) | yes | two-sided, or device time |

Notes:

- NI order is `wait` → `ReadAnalogF64` → counter read → `end()` (`ni/read_task.h:341-366`,
  `ni/hardware/daqmx.cpp:107-119`). The `now()` in `end()` is already an upper stamp
  for the counter. One more `now()` before the counter read gives a bracket.
- I believe "acquired" counts samples in the host buffer, so a lower bound still needs a
  max on-board and transfer latency (vendor docs, not verified).
- Modbus finding: the pre-sleep start puts the stamp about (P − r)/2 before the request,
  where P is the sample period and r the read time (derived). At 10 Hz that is ~50 ms
  early and outside the request window. No issue tracks it.
- EtherCAT readers skip engine epochs silently (`ethercat/engine/reader.cpp:58-69`,
  `:84`) and stamp a batch with `now()` before and after (`ethercat/read_task.h:214-233`).

## 4. Failure and bug history

GitHub issues: the requested search returned nothing about Driver timing (only old
Drift-library and Core items). The history lives in commits, PRs and Linear.

- `c65c80ec39` (v0.39, 2025-03): open-loop clock. Anchor at reset, then `+n*period` per
  read, no correction. Stamps drifted from the host clock.
- SY-2200, #1173 (2025-03-18): NI reads switched to overwrite unread samples and read
  from the most recent sample, which skips backlog.
  https://github.com/synnaxlabs/synnax/pull/1173
- SY-2240, #1186, `ae4ea984c3` (2025-03-27), "Fix Analog Sampling Drift": advance the
  clock by the `TotalSampPerChanAcquired` delta; drop most-recent reads.
  https://github.com/synnaxlabs/synnax/pull/1186
- SY-2245, #1189, `5e61703db2` (2025-03-28): "Gives up on using a stable waveform from
  the device and instead calculates timestamps using the system clock."
  https://github.com/synnaxlabs/synnax/pull/1189
- SY-2247, #1190, `57e4e6f703` (2025-03-31, Urgent, Bug): the current PID clock, back
  cap, and `correct_skew`. "Fourth time is the charm."
  https://github.com/synnaxlabs/synnax/pull/1190. The 0.40 notes say v0.39 "introduced
  a few performance issues related to task timing", and that tasks with a high stream
  rate used to "silently fall behind [and] drop data samples", which the backlog
  warning now reports (`site/docs/src/pages/releases/0-40-0.mdx:26-40`).
- SY-3310, #2843 (2026-08): OPC UA array blocks were stamped from read return forward,
  one block ahead of the wall clock, and an inclusive linspace put 5 samples 50 ms apart
  on a 40 ms period. A restart inside that lead opened a writer inside stored data; the
  Core rejected it and the task stopped for good. It recurred in CI on rc with 6 to
  9.5 µs margins. Residual risk named: a backward wall clock step.
  https://github.com/synnaxlabs/synnax/pull/2843
- SY-4693, #2788 (2026-08): the loop timer measured from wake time, so a 50 Hz Modbus or
  OPC UA task ran at 43 to 44 Hz; worst case -44.6%, now -1.3%.
  https://github.com/synnaxlabs/synnax/pull/2788
- SY-4914, #2994 (2026-09): the PID convergence test was flaky (random jitter, a
  two-window comparison with no transient in it). It is now bounded by the offset it
  removes, with a fixed jitter generator. https://github.com/synnaxlabs/synnax/pull/2994
- SY-5057 (open, 2026-10): an Arc writer opens at "now"; a Driver whose clock is 50 ms
  behind the Core stops a Core Arc task on its first forwarded write.
  https://linear.app/synnax/issue/SY-5057
- SY-2097 and SY-3198: client-to-Core skew warnings (detect only).

## 5. Recommendations for ESTIMATE FIT and `stamp`

1. Anchor bounds to the newest index the host knows, not the last index returned.
   Upper bound: index `acquired - 1` happened before the stamp after the counter read.
   Propagate to earlier indices with the slowest rate inside `Drift`. Evidence: NI and
   LJM give these counts on every read today and use them only for warnings (§3); the
   backlog is exactly the slack in a read-return bound.
2. Make the lower bound explicit and per connector. A measurement is two-sided only
   with a bracketed device reading plus a declared max transfer latency, or with a
   device timestamp. Otherwise it is one-sided. Evidence: every Synnax hardware path is
   one-sided (§1); the only two-sided stamps are request midpoints (OPC UA unary, HTTP).
3. Give `stamp` a device-time input. Index to device time from LabJack
   `STREAM_START_TIME_STAMP` plus scan rate, DAQmx `FirstSampTimestamp`, EtherCAT DC, OPC
   UA source time, or HTTP time fields. Map device time to mesh time with bracketed
   reads (LabJack `CORE_TIMER`, EtherCAT send and receive). Evidence: §3; all reachable,
   none used.
4. Take the true nominal rate from the device. Use LJM's returned scan rate and DAQmx's
   coerced rate, so `Drift` covers only the oscillator. Otherwise a requested-versus-
   actual error eats the drift budget and causes false gaps. Evidence:
   `labjack/read_task.h:696-704`.
5. Feed known discontinuities to the fit as inputs: counter went backwards (device
   restart, `daqmx.cpp:143-146`), counted lost scans (LJM dummies keep the index true),
   unknown loss (DAQmx overwrite) and a lateness flag. A known index jump keeps the fit;
   unknown loss breaks it. Evidence: §2; Synnax absorbs all of these at 1% per read.
6. Choose a point stamp that never exceeds its upper bound and never goes back, in a
   fit and across fits; start a new fit strictly after the last emitted stamp. Evidence:
   SY-3310 (stamps ahead of the clock killed a restart); the back cap
   (`sample_clock.h:100-108`); the test that allows an end 0.4 s ahead of host time
   (`sample_clock_test.cpp:184-211`).
7. Take host stamps from a monotonic clock and map to mesh time once, so an OS clock
   step cannot read as a device jump. Evidence: `telem.h:398-408`, PR #2843 residual
   risk, SY-5057.
8. Bracket the start command. Host stamps around `StartTask` or `eStreamStart` bound
   sample 0 (with a declared start latency), which removes the warm-up bias of a
   first-read anchor. Evidence: `ni/read_task.h:296-308`, `:341`;
   `labjack/read_task.h:697-704`.
9. Index samples, never stream periods; carry a static per-channel offset within a scan
   if sub-sample alignment matters. Evidence: `sample_clock.h:182-189`; one shared
   series per task (`:219-241`).
10. For request and response protocols, stamp the request alone: lower stamp just
    before send, upper stamp just after receive. Evidence: the Modbus pre-sleep
    midpoint (§3 note); OPC UA unary and HTTP do it right.
11. Test against a simulated truth: device clock with drift, a latency distribution,
    stalls, overruns, restarts and host steps. Assert that truth lies inside the
    estimated band. Evidence: Synnax tests check period error with non-production gains
    and never check absolute error (§1); the one statistical test was flaky (SY-4914).
12. Keep Synnax's one real goal: alignment of hardware-timed data with software-timed
    data and commands on the same host (`tasks.mdx:188-192`). Mesh time serves it
    better than a host-anchored low-pass, but only if every connector, software-timed
    ones included, stamps through the same module.
