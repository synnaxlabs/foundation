# Foundation connector components and inversion audit (research fork 12)

Scope: Part A (a component library for connectors), Part B (an inversion audit of every
boundary, plus the seating of each feature), Part C (decisions). Inputs: the Foundation
decision log through BQ5 LOCKED and the seating requirement, forks r1, r2, r4, r5, r6,
r7, and r8, the Synnax driver source and history, and the prior art in A.2.

Evidence marks:

- `path:line` is the Synnax repo working tree on 2026-10-04. A commit hash names the
  change that added the code.
- [unmerged] is branch `sy-4972-add-arinc-429-and-mil-std-1553-integrations-and-move-modbus`,
  which is not on `main`.
- [one source] marks a claim that rests on one source that is not the owner's own code or
  docs (an essay, a forum post, a blog).

---

## 0. Summary

- The Synnax driver has one shared loop: `pipeline::Acquisition` calls `Source::read`.
  Each integration that did not fit built a side mechanism behind `read`: its own
  thread, its own retry layer, its own pacing mode, or its own sharing registry. Nine
  strains recur (A.1).
- Prior art splits in two. Systems with a framework-owned loop (Telegraf, Kafka Connect,
  Redpanda Connect, ros2_control, ROS 2 executors) each added a second path for pushed or
  self-timed sources, and a queue to bridge it. Systems where the component owns its
  loop (Vector, OpenTelemetry receivers, tower, embedded-hal, Linux drivers after the
  "midlayer mistake") put the shared logic in helpers that the component calls (A.2).
- Catalog headline: 14 small modules in `connector` (`cancel`, `pace`, `clock`, `retry`,
  `endpoint`, `link`, `drive`, `thread`, `queue`, `cycle`, `status`, `run`, `out`,
  `calc`) and 7 ready-made compositions in `compose` (`groups`, `polled`, `clocked`,
  `pushed`, `cyclic`, `out`, `calc`). Each composition uses only public modules, so a
  kind that does not fit copies one and changes it (A.3).
- `ctx` holds per-run capabilities tied to the connector's identity. The kind's `&self`
  holds process-lifetime dependencies that `node` injects: endpoint registries, dialers,
  vendor libraries (A.4).
- Inversion audit: layers 1 and 2 are already library-shaped, with four exceptions:
  `connector-status` reads layer-2 internals from layer 3 (this breaks C1), `time` owns
  device handles and a private loop, `buffer` can own a private flusher, and trace (f)
  says `mesh` "notifies" its consumers. Fix all four (B.2).
- Seating: status, calculations, discover, plan, and the orchestration of apply and
  upgrade can sit on the public surface. The control gate, access enforcement, the
  re-index seal, standby takeover, retention trimming, and the time estimate must stay
  inside `home`, `buffer`, `mesh`, or `time`. The standby copy path can be its own
  component on two narrow internal calls (B.4).

---

## Part A. Connector components

### A.1 Where the Synnax driver framework strained

The shared framework: `common::ReadTask` wraps a `common::Source` and adds tare, status,
and error reclassification (`driver/common/read_task.h:107-161`). `pipeline::Acquisition`
owns the loop and calls `Source::read` (`driver/pipeline/acquisition.cpp`).
`common::WriteTask` runs a `pipeline::Control` for commands and a second
`pipeline::Acquisition` for state (`driver/common/write_task.h:183-214`).
`common::SampleClock` has a software and a hardware variant
(`driver/common/sample_clock.h`). `x::loop::Timer` paces, `x::breaker::Breaker` retries,
`common::ScanTask` discovers, and `task::Manager` runs everything.

#### NI DAQmx

- Device setup is vendor-specific. DAQmx refuses a second task with a live task's name,
  so start resets the old reader before it creates a new task
  (`driver/ni/read_task.h:297-313`, `driver/ni/write_task.h:192`). SY-4603 moved
  hardware claims from configure to start for NI, LabJack, and Modbus (113541d706, 1181
  lines inserted).
- Pacing mode is a per-group decision: digital channels without a timing source, and
  counters, are software timed (`driver/ni/read_task.h:82-85`), and `sample_clock()`
  picks one of the two clock components (`:241-252`). The clock component worked here.
- Cancellation cannot reach a blocked read: every read waits with
  `DAQmx_Val_WaitInfinitely` (`driver/ni/hardware/daqmx.cpp:86`, `:110`, `:169`). The
  pipeline checks its stop flag only between reads.
- Recovery happens inside `read`: on `REQUIRES_RESTART` errors the source restarts the
  task, resets the sample clock, and drops the frame (`driver/ni/read_task.h:354-364`).
  The pipeline breaker only knows how to retry the whole run.
- Timestamps: the hardware clock uses a proportional controller (k_p 0.01, a maximum back
  correction of 0.5) against host time (`driver/common/sample_clock.h:74-139`,
  a25b369ba5). Skew warnings apply only to hardware-timed groups
  (`driver/ni/read_task.h:346-348`; SY-3096 c4b9b8e4f6 turned skew tracking off for
  software-timed tasks).

#### LabJack

- A third pacing mode: LJM cannot stream thermocouples, so `UnarySource` paces with the
  vendor's `StartInterval` and `WaitForNextInterval` and stamps with `TimeStamp::now`
  (`driver/labjack/read_task.h:516-605`). The two `SampleClock` variants do not model
  vendor pacing.
- Endpoint sharing needed a registry: one handle per serial number in a `weak_ptr`
  cache, with opens serialized because "LJM returns the same handle when an open device
  is opened again, so two racing opens of one device would let the loser's close kill
  the winner's connection" (`driver/labjack/device/device.h:280-284`, `:332`).
- Recovery inside `read` again: `restart(force)` releases the claim and acquires a fresh
  handle (`driver/labjack/read_task.h:676-706`); `TEMPORARILY_UNREACHABLE` triggers it
  (`:737`). Two backlog warnings, one for the device and one for LJM (`:380-382`).
- Outputs need state restore: start writes the current state to the device
  (`driver/labjack/write_task.h:181-210`), and after an error the task flushes the whole
  state, not only the changed values (`:236-245`).

#### Modbus

- No sharing on purpose: "Each call creates a fresh connection. Connections are not
  cached or shared to avoid thread-safety issues (libmodbus is not thread-safe) and
  stale connection problems when servers restart" (`driver/modbus/device/device.h:167-168`).
  Start takes a fresh connection so a server restart "cannot leave the task reading from
  a dead socket" (`driver/modbus/read_task.h:361`). Cost: one connection per task, and
  the scan task opens one more per device on each scan (SY-3330 2fe09c703e).
- The per-sample loop lives inside `read`, with a midpoint stamp
  `end - (end - start) / 2` (`driver/modbus/read_task.h:396`). The one-frame-per-read
  contract pushed the inner loop into the kind.
- [unmerged] The team is already pulling components out of kinds:
  `bus::Registry::acquire(key, settings, create)` returns a config error when "another
  task has the device open with different settings", and the last release closes it
  (`driver/bus/registry.h:38-60`). `bus::Connection` gives a FIFO `Guard` lock, reopens
  on demand, and counts `opens()` so a reader drops bytes from a dead connection
  (`driver/bus/connection.h:28-56`). `bus::read` drains before a query and holds the
  connection from query to reply (`driver/bus/read.h:79`).

#### OPC UA

- Push was never built: `driver/opcua` has no subscriptions or monitored items (zero
  matches for `MonitoredItem` or `CreateSubscription`). Every read task polls the Read
  service. A subscription needs the client's event loop driven and delivers data in
  callbacks, which a pull-only `read` cannot host without a bridge.
- A second retry layer beside the pipeline breaker: a per-endpoint circuit breaker
  (threshold 3, cooldown 5 s) and serialized connection creation, so reconnect storms do
  not exhaust the server's session table (`driver/opcua/connection/pool.cpp:99-174`,
  SY-3902 0762038a1f). This came after a pool deadlock froze the whole driver (SY-3896
  7a4e252a80).
- Array reads needed their own stamping: blocks are stamped with the window the read took
  (SY-3310 f599d8faea).

#### EtherCAT

- The engine runs its own real-time cycle thread (`Engine::run`,
  `driver/ethercat/engine/engine.cpp:20`) and publishes inputs through a seqlock. All of
  this sits behind `Source::read`.
- Many tasks share one master through registrations. Each reader or writer open stops the
  cycle, deactivates the master, registers the PDOs of every task again, and restarts
  (`Engine::reconfigure`). The cycle period becomes the fastest registered rate
  (`update_cycle_time`, `engine.cpp:170`).
- The reader waits with a fixed timeout and returns `CYCLE_OVERRUN`, or
  `ENGINE_RESTARTING` while another task reconfigures
  (`driver/ethercat/engine/reader.cpp:72-78`).
- The read task decimates the engine rate and stamps a batch evenly between two
  `TimeStamp::now()` calls (`driver/ethercat/read_task.h:209-225`), not with the cycle
  times that the engine knows.

#### HTTP

- A shared `Processor` owns a curl multi event loop on its own `io_thread`; task threads
  block on futures (`driver/http/processor/*.h:38`, `:117`).
- The retry policy needed special values: 761 retries at a 2 minute maximum interval,
  about one day (`driver/http/read_task.cpp:422-423`). SY-4823 (f7b08b6a75) also made
  `ReadTask` report temporary hardware errors as warnings with the next retry time
  (`driver/common/read_task.h:135-145`) and reset the breaker on an empty frame
  (`driver/pipeline/acquisition.cpp:143-146`).

#### Arc

- Arc skipped `common::ReadTask` and `common::WriteTask` and composed
  `pipeline::Acquisition` and `pipeline::Control` directly (`driver/arc/task.h:78-79`,
  `:215-237`). The lower parts were the reusable layer; the upper "framework" was not.
- `Acquisition` grew flags for this one caller: `err_on_unauthorized` (SY-3817
  90613a3b0a) and `open_eagerly` (`driver/pipeline/acquisition.cpp:53-80`), plus an
  `Authorities` out-parameter on `Source::read` (SY-3572 05d3b8a0c6).
- Arc has a second timer implementation with five modes (busy wait, high rate, RT event,
  hybrid, event driven) (`arc/cpp/runtime/loop/loop.h:71-84`), beside `x::loop::Timer`.
- The Arc bypass bus (SY-3512 b0612a81ec) keeps a second local copy of control state.

#### Shared write path, scan, and task runtime

- `common::Sink` is both a sink and a source (`driver/common/write_task.h:33`). State is
  an echo of the last command, emitted at a fixed rate with `TimeStamp::now`, through a
  mutex and condition variable shared by two pipeline threads (`:47-53`, `:100-126`).
  Devices with read-back cannot report it through this path.
- `ScanTask` runs a second thread to stream device changes
  (`driver/common/scan_task.h:201`, `:244-261`) and dedupes NI device locations
  (`:454`).
- `task::Context` hands every task the full Synnax client plus the bus and the control
  states (`driver/task/task.h:60`, `:70-73`): an ambient capability bag.
- `task::Manager` detaches worker threads that do not finish in time
  (`driver/task/manager.cpp:385-386`). A stuck vendor call leaks a thread.
- `x::breaker::Breaker` is three things: a retry backoff, a cancellation flag, and an
  interruptible sleep (`x/cpp/breaker/breaker.h`).
- `x::loop::Timer` has three regimes by rate. The middle regime sleeps with `sleep_for`,
  which stop cannot interrupt (`x/cpp/loop/loop.h:69-86`). Two fixes on 2026-08-21:
  anchor to the deadline (SY-4693 4c7ab913a2) and jitter diagnostics for the Windows
  timer (SY-4694 cfb402d224).

#### Themes

| # | Strain | Where | Component that takes it (A.3) |
| --- | --- | --- | --- |
| 1 | Device setup and teardown are vendor-specific | NI names, LabJack restore, SY-4603 | the kind's own `run` |
| 2 | Four pacing modes, not two: software timer, hardware clock, vendor interval, bus cycle | NI, LabJack, Modbus, EtherCAT, Arc | `pace`, `clock`, `cycle`, kind code |
| 3 | Timestamps: fit, midpoint, window, cycle | all | `clock` |
| 4 | Retry scope: tick, group, whole run | NI, LabJack, OPC UA, HTTP | `retry` and one handler per error class |
| 5 | Shared endpoints and media | LabJack, Modbus, OPC UA, EtherCAT, bus [unmerged] | `endpoint`, `link` |
| 6 | Extra threads | EtherCAT, HTTP, scan, write task, Arc | `thread`, `queue` |
| 7 | Pushed data | OPC UA (never built), Arc | `queue`, `compose::pushed` |
| 8 | Output state: echo vs read-back, restore on reconnect | all write tasks, LabJack | `out` |
| 9 | Ambient context and weak cancellation | `task::Context`, `Breaker`, `Manager` | `ctx`, `cancel` |

### A.2 Prior art

#### Linux kernel: the "midlayer mistake"

- Thesis: "Every subsystem that supports multiple implementations (or drivers) should
  provide a very thin top layer which calls directly into the bottom layer drivers, and a
  rich library of support code that eases the implementation of those drivers." Common
  functionality "should instead be provided as library routines which can used,
  augmented, or ignored by each bottom level driver independently."
- Forcing case: the SCSI midlayer imposed one path on every device. The block layer
  turned the elevator "from a midlayer which is imposed on every device into a library
  function which is available for devices to call upon."
- Lesson: this is BQ5 almost word for word. The supervisor is the thin top layer;
  `connector` is the library. [one source: Neil Brown's essay, LWN 2009]

#### Petricek: libraries, not frameworks

- "When using a framework, the framework is in charge of running the system... When using
  a library, you are in charge." Two frameworks cannot be nested; libraries compose. Advice:
  avoid functions that take many callbacks, and "provide layers" (a low-level explicit API
  plus higher-level helpers built on it).
- Lesson: compositions are the higher layer; the modules are the explicit layer. [one
  source: essay]

#### OpenTelemetry Collector

- Component: a receiver with `Start(ctx, host)` and `Shutdown`. The receiver owns its own
  background work; `Start` returns quickly; `Shutdown` must cancel background work and be
  safe without `Start`.
- Shared layer: `scraperhelper` is an optional controller that owns a ticker and calls
  scrapers; it embeds a `ControllerConfig` and has a test override for the ticker.
  Push receivers (OTLP servers) do not use it.
- Forcing case: pull and push receivers are both common, so the core stays out of the loop
  and offers the ticker as a helper.

#### Telegraf

- Component: `Input.Gather(Accumulator)`, which the agent calls "every agent.interval".
  `ServiceInput` adds `Start(Accumulator)` and `Stop()` for plugins that "operate a
  background service".
- Loop owner: the agent, except for service inputs, which own theirs.
- Forcing case: listeners and consumers (statsd, MQTT, Kafka) cannot fit `Gather`. The
  docs say service inputs are "substantially more complicated", "require threads and
  locks", and "should be avoided unless there is no way". The framework-owned loop made
  the push case second class.

#### Kafka Connect and Debezium

- Component: `SourceTask.poll()`. The worker owns the loop, offsets, and lifecycle. `poll`
  "should block but return control to the caller regularly (by returning null) in order
  for the task to transition to the PAUSED state"; `stop` "may be invoked from a
  different thread than poll()".
- Forcing case: Debezium reads a database log that pushes on its own thread. Its
  `ChangeEventQueue` is "a handover point between producer threads (e.g. MySQL's binlog
  reader thread) and the Kafka Connect polling loop". It applies back pressure, and a
  producer error is stored and "Upon the next call to poll(), that exception will be
  raised" (`debezium-connector-common/.../ChangeEventQueue.java`, class Javadoc).
- Second forcing case: exactly-once sources (KIP-618) needed the task to define
  transaction boundaries, so the framework added a `TransactionContext` capability.
- Lesson: a framework-owned loop collects bridges (a queue, a producer thread, an error
  relay) and new capabilities over time. Foundation's `queue` component is this bridge,
  offered as a part, not forced.

#### Vector

- Component: `SourceConfig::build(&self, cx: SourceContext) -> Source`, where
  `Source = BoxFuture<'static, Result<(), ()>>` (`src/config/source.rs:92`,
  `lib/vector-core/src/source.rs:3`). The source is one future that owns its loop.
- Context: `SourceContext` carries `out: SourceSender`, `shutdown: ShutdownSignal`, proxy
  settings, the acknowledgement flag, schema options, and an `extra_context` "shared
  across all components" (`src/config/source.rs:131-151`).
- Shared layer: helpers in `src/sources/util`, for example `http_client::call`, which
  "calls one or more urls at an interval" with a timeout (`http_client.rs:189-206`), plus
  socket servers and framing.
- Lesson: the closest match to BQ5. Its one weak spot is `extra_context`, an open bag;
  Foundation's `ctx` should stay a closed list.

#### Redpanda Connect (Benthos)

- Component: `service.Input { Connect, Read, Close }`. The framework owns reconnects:
  `Connect` "will be continuously called with back off until a nil error is returned",
  and `Read` returns `ErrNotConnected` to go back to `Connect`
  (`public/service/input.go`).
- Forcing case: push clients. The MQTT input fills an internal channel from the client's
  callback and `Read` drains it (`internal/impl/mqtt/input.go:145-234`). Retry of
  failed messages is a wrapper (`AutoRetryNacks`), which is already a component.
- Lesson: a framework reconnect loop fits request and reply clients; push clients still
  need a bridge queue.

#### tower

- Component: `Service { poll_ready, call }` and `Layer<S> { fn layer(&self, inner: S) ->
  Self::Service }`. The caller owns the loop. Timeouts, retries, rate limits, and load
  shedding are layers.
- Cost: the contract is subtle. Implementations "are permitted to panic if `call` is
  invoked without obtaining `Poll::Ready(Ok(()))`", and a cloned service may not be
  ready.
- Lesson: compose cross-cutting behavior as wrappers the kind chooses, but keep them
  concrete. A generic middleware stack is more machinery than connectors need.

#### embedded-hal 1.0 and embedded-hal-bus

- Component split: `SpiBus` is "exclusive ownership over the whole SPI bus"; `SpiDevice`
  is "ownership over a single SPI device selected by a CS pin in a (possibly shared)
  bus", and its transaction ensures "no other transaction can be opened on the same bus".
  Drivers depend on `SpiDevice`.
- Shared layer: `embedded-hal-bus` gives the sharing choices (`ExclusiveDevice`,
  `RefCellDevice`, `CriticalSectionDevice`, `MutexDevice`, `AtomicDevice`). The
  application picks one at the wiring site, not the driver.
- Lesson: separate the shared medium from the per-device handle, and pick the sharing
  mechanism where the parts are wired. This is `endpoint::Shared` and the [unmerged]
  `bus::Connection`.

#### ROS 2 executors and ros2_control

- Component: callbacks of subscriptions, timers, and services. The executor owns the spin
  loop. Callback groups are mutually exclusive or reentrant.
- Forcing case: the docs list why the executors are "not suitable for real-time
  applications": mixed scheduling semantics, priority inversion, "no explicit control
  over the callbacks execution order". One fix inverts the loop: the rclcpp `WaitSet`
  "allows waiting directly on subscriptions, timers... instead of using an Executor" for
  "deterministic, user-defined processing sequences". The newer events executor keeps an
  unbounded queue ("no limit to the number of events").
- ros2_control: the controller manager calls each hardware component's
  `read(time, period)` and `write(time, period)` in its loop
  (`hardware_component_interface.hpp:240-288`). A slow sensor "affects the periodicity
  of the controller_manager control loop", so components gained `is_async` and a
  `detached` policy in which "the hardware component will manage its own timing".
- Lesson: two mature robotics frameworks added an exit from their own loop for the exact
  edge cases in A.1 (timing and slow devices).

#### EPICS asyn

- Component: a port driver registered with `asynManager`. With `ASYN_CANBLOCK`,
  "asynManager creates a thread for the port"; non-blocking drivers are called directly.
  `ASYN_MULTIDEVICE` adds addresses on one port. `queueRequest` serializes access, so
  "only one callback is active at a time". Pushed data uses `registerInterruptUser`.
  Interpose interfaces (`asynInterposeEos`) add framing to a port that lacks it.
- Forcing case: shared serial and GPIB buses with blocking I/O and many instruments.
- Lesson: long use in accelerator controls supports three of our parts: one thread per blocking endpoint,
  one ordered queue per shared medium, and framing as a wrapper.

#### Ignition SDK (with Eclipse Milo)

- Component: a `Device` with callbacks (`onDataItemsCreated`, `onDataItemsModified`,
  `onDataItemsDeleted`, `onMonitoringModeChanged`). The SDK example hands all four to
  Milo's `SubscriptionModel`, an optional helper, and runs its own startup and shutdown
  tasks (`opc-ua-device/.../ExampleDevice.java:28-48`, `:202-218`).
- A forum write-up calls the example's timer-based updates naive at scale [one source].
- Lesson: even a callback framework ships the hard part as a helper the device may
  replace.

#### Synthesis

| System | Loop owner | Push or self-timed sources | Shared logic lives in |
| --- | --- | --- | --- |
| Telegraf | framework | second interface (`ServiceInput`) | the agent |
| Kafka Connect | framework | bridge queue and thread (Debezium) | the worker |
| Redpanda Connect | framework | bridge channel | the framework, plus wrappers |
| ros2_control | framework | `detached` async components | controller manager |
| ROS 2 rclcpp | framework | `WaitSet` exit | executor |
| Ignition | framework callbacks | helper (`SubscriptionModel`) | helpers |
| Vector | component | native | `sources/util` helpers |
| OpenTelemetry | component | native | `scraperhelper` |
| tower | caller | native | layers |
| embedded-hal | driver | native | `embedded-hal-bus` |
| EPICS asyn | manager for blocking ports | interrupt callbacks | `asynManager`, interpose |
| Linux block layer | driver | native | library routines |

Every framework-owned loop in the table grew an exit. None of the component-owned
designs needed a second interface for push.

### A.3 The catalog

Rules for every component:

1. A concrete type with one job. A trait only where real runtime polymorphism exists
   (`link::Link`, `drive::Machine`, `cycle::Bus`).
2. No component reaches for another. The kind (or a composition) wires them in `run`.
3. No component holds the hub. Components that write take a session or a frame builder
   from the caller.
4. Time comes from `env::Clock` (monotonic) and `ctx.now()` (mesh time), never the OS,
   so every component runs in simulation (Q19, Q20).
5. Each component is safe to cancel at every wait.

#### Modules

| Module and type | Job | Edge cases it covers (A.1) |
| --- | --- | --- |
| `cancel::Token` | Cancels a run or a group, with child tokens and hooks that unblock a blocked vendor call. | DAQmx infinite waits, `Breaker` conflation, detached threads |
| `pace::Timer` | Ticks at a period anchored to deadlines, reports missed and late ticks, and picks sleep, hybrid, or spin by period. | SY-4693 anchor, SY-4694 Windows, non-cancellable sleep, Arc's second timer |
| `clock::Software` | Stamps one sample from the mesh-time interval around its request (midpoint, with the half width as error). | Modbus midpoint, OPC UA unary reads |
| `clock::Window` | Stamps a block of samples evenly across the window its read took. | OPC UA arrays (SY-3310), EtherCAT batches |
| `clock::Fit` | Fits a device oscillator to mesh time from the lower envelope of read-return times; never steps back; reports skew (r6 Q5). | P controller, back correction, skew warnings |
| `retry::Backoff` | Waits with capped exponential backoff and jitter; resets on success. | HTTP 761-retry config, breaker reset hacks |
| `retry::Breaker` | Opens after N failures on one endpoint and probes after a cooldown. | OPC UA reconnect storms (SY-3902) |
| `endpoint::Registry<T>` | Keeps one live endpoint per key, serializes opens, refuses a second open with different settings, closes on the last release. | LabJack handle race, OPC UA session pool, EtherCAT master pool, bus registry [unmerged] |
| `endpoint::Shared<T>` | Gives FIFO turns on one endpoint, so a query and its reply stay together; reopens on demand and exposes a generation number. | Modbus multi-drop, `bus::Connection` [unmerged], asyn `queueRequest` |
| `link::Tcp`, `Udp`, `Serial`, `Can` | Opens one medium with its settings (timeouts, keepalive, no-delay, buffer sizes, baud, parity, RS-485 mode, CAN bit timing) and reads and writes with deadlines; `drain` drops stale bytes. | socket setup, dead sockets after restarts, CAN timing [unmerged] |
| `drive::Machine` and `drive::exchange` | Drives a sans-I/O protocol core over a link: transmit, receive, deadlines, events. | r7's own Modbus, MQTT, Kafka cores |
| `thread::Dedicated<T>` | Owns a value on its own OS thread (name, priority, cores, locked memory), runs calls or a loop there, and joins on cancel; never detached. | DAQmx, LJM, EtherCAT RT thread, curl thread, `Manager` detach |
| `queue::Bounded<T>` | Moves items between a thread or callback and the shard, with back pressure or drop-oldest plus a gap count, and relays a producer error. | Debezium-style bridge, OPC UA subscriptions, MQTT callbacks |
| `cycle::Engine` | Runs a cyclic process-data exchange on a dedicated thread, publishes each cycle's input image with its cycle number and mesh time, and latches outputs at the next cycle. | EtherCAT engine, seqlock, fixed 200 ms waits |
| `status::Reporter` | Writes connector and group status; dedups and rate-limits repeats; never delays a transition. | `StatusHandler` rate limit, skew and backlog warnings |
| `run::Commands` | Yields start and stop commands per group from the run command channel and writes the ack (Q14). | `exec` start and stop, start when already running |
| `out::Latest` | Keeps the latest commanded value per channel from a reader session; lists changed values; gives a full snapshot for restore. | write buffers, LabJack restore and full flush |
| `out::State` | Writes the output state channel as read-back when the device has it, else as an echo of the accepted command. | echo-only state in `common::Sink` |
| `calc::Align` | Joins several input streams as-of onto one trigger stream. | calculations over several indexes |

`types::frame::Builder` (layer 1, backed by `hub.block`) is the frame builder. It is not
a connector component.

#### Signatures

```rust
pub mod cancel {
    pub struct Token;
    impl Token {
        pub fn child(&self) -> Token;
        pub fn is_cancelled(&self) -> bool;
        pub async fn cancelled(&self);
        /// Runs `f` once, on the cancelling thread. Use it to unblock a vendor call.
        pub fn on_cancel(&self, f: impl FnOnce() + Send + 'static) -> Hook;
    }
}

pub mod pace {
    pub enum Mode { Sleep, Hybrid { spin: Duration }, Spin }
    pub struct Tick { pub n: u64, pub missed: u64, pub late: Duration }
    pub struct Timer;
    impl Timer {
        pub fn new(clock: env::Clock, period: Duration, mode: Mode) -> Self;
        pub async fn tick(&mut self, cancel: &cancel::Token) -> Option<Tick>; // on a shard
        pub fn wait(&mut self, cancel: &cancel::Token) -> Option<Tick>;       // on a thread
    }
}

pub mod clock {
    pub struct Stamp { pub at: TimeStamp, pub error: Duration }
    pub struct Software;  // stamp(before: Interval, after: Interval) -> Stamp
    pub struct Window;    // stamps(before: Interval, after: Interval, n: usize) -> impl Iterator<Item = Stamp>
    pub struct Fit;       // observe(sample: u64, returned: Interval); stamp(sample: u64) -> Stamp;
                          // skew() -> i64; reset()
}

pub mod retry {
    pub struct Backoff;   // new(cfg, rng); async wait(&cancel) -> bool; reset()
    pub struct Breaker;   // allow() -> bool; success(); failure()
}

pub mod endpoint {
    pub struct Registry<T>;  // async acquire(key, settings, open) -> Result<Lease<T>, Error>
    pub struct Lease<T>;     // derefs to Shared<T>; the last drop closes the endpoint
    pub struct Shared<T>;    // async lock() -> Guard<T> (FIFO); generation() -> u64
}

pub mod link {
    pub trait Link {                       // Tcp, Udp, Serial, Can, and the simulator
        async fn read(&mut self, into: &mut [u8], deadline: Instant) -> Result<usize, Error>;
        async fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), Error>;
        fn drain(&mut self) -> Result<usize, Error>;
    }
    pub struct Dial;                       // real or simulated opener; injected on the kind
}

pub mod drive {
    pub trait Machine {                    // sans-I/O protocol core, quinn-proto shape
        type Event;
        fn transmit(&mut self, now: Instant) -> Option<Bytes>;
        fn receive(&mut self, now: Instant, bytes: &[u8]);
        fn deadline(&self) -> Option<Instant>;
        fn expire(&mut self, now: Instant);
        fn event(&mut self) -> Option<Self::Event>;
    }
    pub async fn exchange<M: Machine>(
        m: &mut M, link: &mut impl link::Link, clock: &env::Clock, cancel: &cancel::Token,
    ) -> Result<M::Event, Error>;
}

pub mod thread {
    pub struct Config { pub name: String, pub priority: Option<u8>, pub cores: Cores,
                        pub memory_locked: bool }
    pub struct Dedicated<T>;  // made by ctx.thread(cfg, open)
    impl<T: 'static> Dedicated<T> {
        pub async fn call<R: Send>(&self, f: impl FnOnce(&mut T) -> R + Send) -> R;
        pub fn run<R: Send>(self, f: impl FnOnce(T, cancel::Token) -> R + Send) -> Join<R>;
    }
}

pub mod queue {
    pub enum Full { Wait, DropOldest }
    pub enum Item<T> { Value(T), Gap { dropped: u64 } }
    pub fn bounded<T>(capacity: usize, full: Full) -> (Sender<T>, Receiver<T>);
    // Sender::push(T); Sender::fail(Error); Receiver::recv().await -> Result<Item<T>, Error>
}

pub mod cycle {
    pub trait Bus {                        // IgH, SOEM, or the simulator
        fn exchange(&mut self, outputs: &[u8], inputs: &mut [u8]) -> Result<u16, Error>;
    }
    pub struct Engine;    // start(ctx, thread::Config, bus, layout, period) -> Result<Engine, Error>
    pub struct Snapshot;  // cycle: u64, at: Stamp, inputs: &[u8]
    pub struct Outputs;   // set(|image: &mut [u8]| ..), latched at the next cycle
}
```

`status`, `run`, `out`, and `calc` are small structs over a session from `ctx`.

#### Error classes, one handler each

C3's shared error set stays. Each class has exactly one handler, so no layer retries
what another layer already retries.

| Class | Meaning | Handler | Action |
| --- | --- | --- | --- |
| `Retry` | This attempt failed; the next one may work. | The composition's tick loop | Count it, mark a gap, warn, back off, continue. |
| `Device` | The device or endpoint is in a bad state. | `compose::groups` (group scope) | Stop the group, back off, open it again. `clock::Fit` resets with a gap. |
| `Config` | The config cannot work. | The supervisor | Error status; wait for a spec change. |
| Any other error that leaves `run` | | The supervisor | Restart `run` with backoff. |

A kind that writes its own loop uses the same classes with the same scopes.

#### Ready-made compositions

Each is a plain `async fn` over public modules and closures, about 50 to 150 lines. Its
source is the guide for a custom `run`.

| Composition | Parts | What it does |
| --- | --- | --- |
| `compose::groups` | `run::Commands`, child `cancel::Token`s, `retry::Backoff`, `status` | Runs one future per group; starts and stops groups on run commands; restarts a group on `Device`. |
| `compose::polled` | `pace::Timer`, `retry::Backoff`, `status`, writer session | Calls the kind's closure each tick; the closure fills and stamps one sample (one value per channel, one time). Software-timed DAQ groups use this too. |
| `compose::clocked` | `ctx.thread`, `queue::Bounded`, `clock::Fit`, `status`, writer | Runs the kind's blocking read loop on a dedicated thread; stamps samples with the fit; reports skew. |
| `compose::pushed` | `queue::Bounded`, `clock::Software` or source time, writer | Writes items that a callback or a client loop pushes (OPC UA subscriptions, MQTT, Kafka). |
| `compose::cyclic` | `cycle::Engine`, writer | Takes every Nth cycle snapshot, decodes it with the kind's closure, and stamps it with the cycle's own time. |
| `compose::out` | reader session, `out::Latest`, `pace::Timer`, `out::State` | Calls the kind's write closure at most at the group rate, then writes state. |
| `compose::calc` | reader sessions, `calc::Align`, writer | Evaluates an expression on aligned inputs and writes one output index (C5). |

#### Kind sketch 1: Modbus TCP, polled

```rust
pub struct Kind {
    ports: endpoint::Registry<link::Tcp>,  // one connection per server for all groups
    dial: link::Dial,                      // real sockets or the simulator (Q19)
}

impl connector::Kind for Kind {
    type Config = Config;

    async fn run(&self, ctx: Context<Config>) -> Result<(), Error> {
        let cfg = ctx.config();
        let port = self.ports
            .acquire(&cfg.address, &cfg.socket, || self.dial.tcp(&cfg.address, &cfg.socket))
            .await?;
        let plans = plan::reads(cfg)?;  // registers sorted and merged per group
        compose::groups(&ctx, async |group, _cancel| match group {
            Group::In(g) => compose::polled(&ctx, g, async |sample| {
                let mut link = port.lock().await;  // a query and its reply stay together
                link.drain()?;
                let before = ctx.now();
                for read in &plans[g.id] {
                    let regs = drive::exchange(
                        &mut modbus::read(cfg.unit, read), &mut *link, ctx.clock(), ctx.cancel(),
                    ).await?;
                    sample.set(read, regs, cfg.order);  // byte and word order
                }
                sample.stamp(clock::Software::stamp(before, ctx.now()));
                Ok(())
            }).await,
            Group::Out(g) => compose::out(&ctx, g, async |latest| {
                let mut link = port.lock().await;
                for w in latest.changed() {
                    drive::exchange(
                        &mut modbus::write(cfg.unit, w), &mut *link, ctx.clock(), ctx.cancel(),
                    ).await?;
                }
                Ok(())
            }).await,
        }).await
    }
}
```

Covered: one connection for all groups (Synnax opened one per task and one per scan); a
dead socket after a server restart is a `Retry` and `Shared` reopens it; stale bytes are
drained; one midpoint stamp per sample with an error bound; deadline-anchored pacing.

#### Kind sketch 2: NI DAQmx, hardware timed

```rust
pub struct Kind {
    daqmx: Arc<daqmx::Library>,  // loaded once by node; no library -> Config error
}

impl connector::Kind for Kind {
    type Config = Config;

    async fn run(&self, ctx: Context<Config>) -> Result<(), Error> {
        compose::groups(&ctx, async |group, _cancel| match group {
            Group::In(g) if g.clock.is_some() => compose::clocked(&ctx, g, |cancel| {
                // On the group's own thread. A new name per open: DAQmx refuses a live name.
                let task = self.daqmx.task(&g.unique_name())?;
                task.channels(&g.channels)?;
                task.sample_clock(g.rate, g.clock.as_ref())?;  // checks the device minimum rate
                let stop = task.stopper();
                let hook = cancel.on_cancel(move || stop.stop());  // ends a waiting read
                task.start()?;
                Ok(daqmx::Reader::new(task, hook, g.samples_per_read()))
            }).await,
            Group::In(g) => {  // digital lines without a clock, counters
                let task = ctx.thread(g.thread(), || self.daqmx.on_demand(g))?;
                compose::polled(&ctx, g, async |sample| {
                    let before = ctx.now();
                    sample.set_all(task.call(|t| t.read_once()).await?);
                    sample.stamp(clock::Software::stamp(before, ctx.now()));
                    Ok(())
                }).await
            }
            Group::Out(g) => {
                let task = ctx.thread(g.thread(), || self.daqmx.output(g))?;
                compose::out(&ctx, g, async |latest| {
                    let values = latest.snapshot();
                    task.call(move |t| t.write(&values)).await
                }).await
            }
        }).await
    }
}
```

Covered: a unique task name per open; cancel ends an infinite read from another thread;
`REQUIRES_RESTART` maps to `Device`, so `compose::groups` restarts only that group and
`clock::Fit` resets with a gap; skew status exists only for clocked groups by
construction; vendor calls never run on a shard (r1 Q4).

#### Kind sketch 3: EtherCAT, cyclic

```rust
pub struct Kind {
    masters: endpoint::Registry<ethercat::Master>,  // one master per network interface
    open: ethercat::Open,                           // IgH on Linux, SOEM elsewhere, or sim
}

impl connector::Kind for Kind {
    type Config = Config;

    async fn run(&self, ctx: Context<Config>) -> Result<(), Error> {
        let cfg = ctx.config();
        let master = self.masters
            .acquire(&cfg.interface, &(), || self.open.master(&cfg.interface))
            .await?;
        let layout = pdo::Layout::plan(&cfg.slaves, &cfg.groups)?;  // fixed for this run
        let engine = cycle::Engine::start(&ctx, cfg.thread.clone(), master, &layout, cfg.period)?;
        compose::groups(&ctx, async |group, _cancel| match group {
            Group::In(g) => compose::cyclic(&ctx, g, &engine, |snap, sample| {
                layout.decode(g, snap, sample)
            }).await,
            Group::Out(g) => compose::out(&ctx, g, async |latest| {
                engine.outputs().set(|image| layout.encode(g, latest, image));
                Ok(())
            }).await,
        }).await
    }
}
```

Covered: all PDOs are registered once per run, so no full-bus reconfigure when a group
starts (Synnax `Engine::reconfigure`); the cycle period comes from config, and plan checks
that each group rate divides it (Synnax used the fastest registered rate); samples carry
the cycle's own time (Synnax stamped batches with `now()`); waits are cancellable, with no
fixed 200 ms timeout; the RT thread settings come from config through `ctx.thread`. One
master per connector follows Q15.

#### Where invariants are enforced

| Invariant | Enforced by | Not by |
| --- | --- | --- |
| One writer per index | `home` at `open_writer`; `plan` rejects two groups on one index as a user error | the connector loop |
| Control authority | `home` through `control` (S11) | connectors, `ctx` |
| Data access | `home` through `access` (Q12); sessions open as the connector's subject | `ctx`, `hub` |
| Timestamps strictly increase per index | `home` write path (A5) | components, which only aim for it |
| One owner per endpoint | `endpoint::Registry` on the kind, OS exclusive open (for example `TIOCEXCL` on a tty), and the supervisor never overlapping two runs of one connector | `hub` |
| Bounded memory per connector | `queue::Bounded` capacities and `hub` session credits (B3) | |
| A run stops when cancelled | the supervisor: it waits for `run` to return, shows "stuck" status after a deadline, and does not start the next run until the old one returns | detached threads |
| A secret reaches only its connector | `ctx.secret` decrypts only names that the connector's config references (Q16) | |

### A.4 The `ctx` capability set

```rust
pub struct Context<C> { /* private */ }

impl<C> Context<C> {
    pub fn config(&self) -> &C;                    // decoded and checked at plan time
    pub fn name(&self) -> &Name;                   // the connector's name, also its subject
    pub async fn writer(&self, g: &InGroup) -> Result<hub::WriterSession, Error>;
    pub async fn reader(&self, g: &OutGroup) -> Result<hub::ReaderSession, Error>;
    pub fn status(&self) -> &status::Reporter;
    pub fn run_commands(&self) -> run::Commands;   // Q14
    pub fn secret(&self, name: &str) -> Result<Secret, Error>;
    pub fn cancel(&self) -> &cancel::Token;
    pub fn now(&self) -> Interval;                 // mesh time (C6)
    pub fn clock(&self) -> &env::Clock;            // monotonic only (Q20)
    pub fn rng(&self) -> env::Rng;                 // replayable in simulation
    pub fn block(&self, len: usize) -> Block;      // pooled bytes (S2)
    pub fn thread<T: 'static>(
        &self, cfg: thread::Config, open: impl FnOnce() -> Result<T, Error> + Send,
    ) -> Result<thread::Dedicated<T>, Error>;      // cores come from the node's plan
}
```

Must not include:

- `Hub`, `Mesh`, `Home`, `Buffer`, `Transport`, or `time::MeshClock` (C1: layer 3 reaches
  the mesh only through `hub`, and `ctx` exposes only scoped parts of it).
- Spec changes or `propose`. Discover returns definitions; `config` writes the files.
- Sessions on channels outside the connector's groups. `ctx` opens sessions as the
  connector's subject, and `home` refuses anything else through `access`. `ctx` adds no
  second check.
- Other connectors' configs, secrets, or endpoints.
- `env::Fs`, the OS wall clock, and spawning onto another shard.
- An open "extra" bag (Vector's `extra_context` is the warning).

Rule for the split between `&self` and `ctx`:

- `&self` (the kind value, built once by `node`) holds process-lifetime dependencies:
  endpoint registries, dialers (real or simulated), and loaded vendor libraries. Every
  run of every connector of that kind shares them, which is how one registry sees all
  users of a serial port.
- `ctx` holds what belongs to one run of one connector: its identity, sessions, status,
  secrets, and cancellation.

Placement: `run`, its groups, and the homes of every index the connector writes should
sit on one shard, so writer sessions never cross cores. r1 measured local writes far
below a cross-core handoff (handoff p99 0.18 to 1 ms on macOS). Today r1 places a
connector on "the shard that owns the indexes it writes", which is ambiguous when a
connector has groups on several indexes. Choose the shard by connector.

---

## Part B. Inversion audit

### B.1 Boundary by boundary

"Library" means the caller drives and the callee returns. "Framework" means the callee
calls back up through callbacks or hook traits. A `Watch` or a stream that the upper
crate polls in its own loop counts as library form.

| Boundary | Control flow today (r8 plus locked changes) | Verdict |
| --- | --- | --- |
| `mesh` -> `raft` | `mesh` drives a sans-I/O Raft (etcd/raft Ready loop, r4) | Library. Keep. r4 rejected openraft because "it owns the control flow and calls our storage and network". |
| `home` -> `control`, `delivery`, `access` | `home` calls pure leaves that return decisions | Library. Keep. Rule: leaves return effects and never call out. |
| any -> `types`, `block`, `spec`, `codec`, `wire` | function calls | Library. Keep. |
| layer 2 -> `env` | traits injected downward (real or simulated) | Dependency injection, not an upward call. Keep. |
| `hub` -> `transport` | `hub` pulls `accept` (BQ1); `transport` never calls up | Already inverted. Keep. |
| inside `transport` | noq-proto sans-I/O, driven by shard loops (r5) | Library. Keep. |
| `home` -> `buffer` | `home` calls `append` and `set_floor`; `buffer` publishes `durable()` as a `Watch`; group commit timing is inside `buffer` | Mostly library. Open point: who drives the disk I/O. See I4. |
| `time` | owns the offset exchange loop and the GPS and PTP device handles | Framework-shaped inside layer 2. See I3. |
| `mesh` -> `blob` | calls | Library. Keep. |
| `mesh` -> its consumers | `watch_spec` and `watch_homes` are `Watch` values; trace (f) says `mesh` "notifies" five consumers | Must stay pulled. See I2. |
| `home` and leases | `home` reads the lease and the time bound on every write | Library. Keep. |
| `hub` -> `home` | local call path | The forward that C1 allows. Keep. |
| layer 3 -> `hub` | sessions are streams the caller polls, and calls | Library. Keep. |
| supervisor -> kind | starts and cancels `run` (BQ5) | Already inverted. Keep it thin. |
| `connector-status` -> layer 2 | a layer-3 kind reads `Watch` values from `time`, `buffer`, `transport`, `mesh`, `home` | Breaks C1. See I1. |
| `config` <- kind schemas | passed as an argument | Library. Keep. |
| `ops` -> `Kind::discover` | a call through the kind table | Library. Keep. |
| OPC UA kind -> open62541 | the kind drives `UA_EventLoop` `run(timeout)` on its own thread (r7) | Library use of a framework-shaped C library. Keep, inside `thread::Dedicated`. |
| kinds -> DAQmx, LJM | blocking vendor calls | Vendor code cannot be inverted. Contain it in `thread::Dedicated`. |

### B.2 Recommended inversions

**I1. Move status collection to layer 4.** r8's `connector-status` is a layer-3 kind that
reads layer-2 `Watch` values, which C1 forbids. Instead, `node` wires a status collector:
it receives each layer-2 crate's `observe()` `Watch` and a `hub` writer session, and
writes the `<node>.*` channels. Layer-2 crates still never write channels (Q11), and layer
3 still sees only `hub`. Cost: each layer-2 crate exposes one `observe()`. Evidence: C1
and Q11 together leave no other seat: a layer-3 kind cannot see the values, and a layer-2
crate must not write channels.

**I2. Upward flow only through pulled values.** No lower crate keeps a list of
subscribers that it calls, and no hook trait is implemented above and called below.
Upward flow is a `Watch` or a stream that the upper crate polls in its own loop. Trace
(f) step 3 then reads: `home`, `transport`, the supervisor, `hub`, and `time` each watch
`mesh`; `mesh` calls nobody. Enforce with the architecture check that already guards
layer imports. Cost: none; the r8 shapes already fit. Evidence: Petricek's composition
argument; r4's reason for rejecting openraft.

**I3. Split `time` into an estimator and injected sources.** The estimator is a pure
state machine: samples in, offset and error bound out (r1 Q5 lists clock offset
estimation as a sans-I/O core). Sources implement a small `time::Source` trait (peer
exchange over `transport`, PTP hardware clock, GPS with PPS, the OS clock), and `node`
wires the list. Device protocol code (NMEA parsing, PHC ioctls) leaves `time` for its own
crates. Cost: one trait with real polymorphism. Evidence: r1 Q5 (sans-I/O cores) and
r6 (sources of different quality feed one estimate); the same split as noq-proto, where
the core takes time as an argument and the loop around it owns the I/O (r5).

**I4. `buffer` is driven, not self-running.** r2 already shapes the engine as sans-I/O
("append these bytes, sync this file") with a driver per OS. The driver should run on the
owning shard where the OS has async file I/O (io_uring), and on a small I/O thread
elsewhere. Group commit timing stays in the engine as a deadline that the driver polls.
The engine owns no timer and no thread. Cost: a per-OS driver, which r2 already plans.
Evidence: r1 notes Tokio file I/O uses the shared blocking pool (default 512 threads)
and Iggy moved away from it.

**I5. Endpoint ownership becomes a component, plus one supervisor rule.** C3's "one
endpoint owned by one task" moves from a framework promise to `endpoint::Registry` (on
the kind) and OS exclusive opens. The supervisor adds one structural rule: it never
starts a run of a connector while the previous run has not returned. Evidence: the
LabJack open race; [unmerged] `bus::Registry`; embedded-hal's bus and device split.

**I6. The supervisor stays the thin top layer.** It does four things: start `run`,
cancel it, handle errors that leave `run` (A.3 table), and never overlap runs. Nothing
else. Evidence: the "midlayer mistake" essay; every framework in A.2 that did more grew
an exit.

### B.3 Where not to invert

- `hub` routing and reconnection. Inverting would push home failover into every kind.
  `hub` is a deep module; keep it whole.
- The `home` write path order (access, control, timestamps, seq, `buffer`, `delivery`).
  One owner of per-index order is the invariant itself.
- The supervisor. Something must start runs. The Linux essay keeps "a very thin top
  layer".
- Group restarts in `compose::groups`. They are optional because they live in a
  composition, so they need no inversion.

### B.4 Feature seating

Public surfaces of the small core:

- Data surface: `hub` (reader, writer, spec, watch, now, block). Layer 3 sees it
  through `ctx`.
- Control-plane surface for layer 4: `mesh` public calls (`spec`, `watch_spec`,
  `propose`, members and their versions, `history`), `blob`, and each layer-2 crate's
  `observe()`.
- Anything else is internal.

**Standby and recovery (seating only; r13 compares the schemes).** Two paths with
different needs.

- Copy path: the standby must store the home's encoded blocks with their original seq,
  so positions and backfill dedup still work after takeover (Q6, Q7, B7). `hub` cannot
  carry this: it decodes for local readers (Q4), and a write through `hub` gets a new seq
  from the destination's write path. So the copy path needs two narrow internal calls: a
  raw mode of `home::subscribe` (encoded blocks, original seq, with a hold) and
  `buffer::append_at(index, seq, block)`. With those, the copy path is its own
  component (`standby`, layer 2, after `home`), and it never touches the write path.
- Takeover path: lease expiry and the homes map swap (`mesh`), a fresh seq block
  (`mesh`), opening the index from the local `buffer` (`home`), and fencing (`home` with
  `time`). This must stay inside `mesh` and `home`: fencing is a check on every write,
  and a separate component would race with it. Trade: correctness over separation.

**Re-index.** The seal is a check in the `home` write path (refuse the channel after the
sealed sample), the epoch record is runtime state in `mesh`, and readers follow epochs in
`hub` routing (Q9). None of these can sit on the public surface without a race between
the seal and the last write. The connector side needs nothing: a spec change restarts
`run` (R12-4), and the new home accepts the channel only after the seal. The fully
separable choice is to forbid re-index and treat a move as a new channel, which is
simpler but breaks name continuity. Keep it in the core; it is one check, one record
type, and the routing rule.

**Retention and holds.** The floor policy (age and size rules from `spec::resolve`,
combined with reader holds) is a pure function, so it can sit beside `delivery`. Holds
are reader state in `delivery`. Trimming stays in `buffer`, which owns the files and must
trim past holds when the disk budget runs out (B1), with a recorded gap. These are two
decision points, but they decide different things: policy in `home`, disk safety in
`buffer`. Boundary: the existing `buffer::set_floor`.

**Time sync.** The estimate and `now()` are core: every timestamp and every lease uses
them, and they sit below `hub`. Sources are separable through `time::Source` (I3).
Sources cannot be connectors: a GPS connector would feed the clock that `hub` (its own
layer) depends on, and the clock must work before `hub` starts. Steering the OS clock
(opt-in) is an effect that `node` wires. Trade: one trait, in exchange for keeping device
code out of `time`.

**Status channels.** Separable (I1): a layer-4 collector on `observe()` values and `hub`
writer sessions. Connector status goes through `ctx.status()`. Exception: `home` writes
an index's control channel itself (Q11), because the channel must follow the gate's
decisions in the same order. Moving it out would let the channel and the gate disagree.

**Access checks.** The decision logic is already separate (`access`, pure, layer 1).
Enforcement stays at the owners (`home` for data, the voters for `apply`, `secret`, and
`admin`). A check outside the owner can be bypassed by a compromised node (Q12). Trade:
none worth taking.

**Control gate.** The decision logic is already separate (`control`, pure, layer 1). The
gate stays in the `home` write path, because the decision and the write order must be
atomic per index (S11). Synnax evidence for the cost of a second path: the Arc bypass bus
keeps its own `control::States` copy (b0612a81ec), so two views of control can drift.

**Calculations.** Separable: a calc is a kind (C5) that uses only `ctx`: reader sessions
on its inputs, `calc::Align`, and one writer. Trade: inputs homed elsewhere cost a network
hop, so placement puts calcs near their inputs (A17); each input picks a latest or
complete reader by its needs.

**Secrets.** Ciphertexts must be replicated, agreed state, so they stay in `mesh` runtime
state as a typed record that only the voters authorize (Q12, Q16). Everything else is
separable: sealing runs in `ops` (layer 4), decrypting in `node`, and delivery through
`ctx.secret`. `mesh` stores opaque bytes and never sees plaintext. Trade: one record type
in `mesh`, in exchange for keeping crypto out of it.

**Upgrades.** The rollout (one node at a time, voters last) is an `ops` and `node`
feature on public calls: propose the desired version, take a lock record, read member
versions from lease renewals (Q18), fetch the binary from `blob`. The mesh version flag
that turns on new formats must reach every encoder (`codec`, `wire`, `buffer`) as an
injected value, because a node must never write a format that a peer cannot read. Trade:
the lease path carries the version field.

**Discover.** Separable: an optional `Kind::discover` that `ops` runs on the node that can
reach the device. It uses the kind's `endpoint::Registry`, so it can reuse a running
session (an OPC UA browse) or report "endpoint busy" for an exclusive link. Drop the
always-on scan loop: health comes from the running connector's status, so no scan opens
extra connections (the Modbus scan did, SY-3330).

**Plan and apply.** `plan` is pure (`config` over files, the current spec, and a runtime
view read through `hub`). The `apply` orchestration (branch order, partial results) sits
in `ops`. The compare-and-swap of branch pointers is the core job of `mesh` and stays
there. Boundary: `mesh::propose(Apply { branch, from, to })` and `blob::put`.

| Feature | Internals touched | Separable | Proposed boundary | Trade |
| --- | --- | --- | --- | --- |
| Standby copy path | `home` subscription (raw mode), `buffer` append at seq | Partly | `standby` component on `home::subscribe(Raw)` and `buffer::append_at` | Two internal calls, so seq identity survives failover |
| Standby takeover | `mesh` lease and homes map, seq block; `home` open and fencing; `time` | No | inside `mesh` and `home` | Correctness: fencing is a check on every write |
| Re-index | `home` seal, `mesh` epochs, `hub` routing | No | one check, one record type, one routing rule | Correctness of order at the seal; the alternative (no re-index) loses name continuity |
| Retention and holds | `delivery` holds, `buffer` trim and budget, `spec::resolve` | Partly | pure floor function; `buffer::set_floor` | Disk safety stays in `buffer` |
| Time sync | `time` estimate, `transport`, `home` fencing | Partly | `time::Source` trait wired by `node` | Simplicity: sources leave `time`; the estimate stays core |
| Status channels | `observe()` of layer-2 crates, `hub` writers | Yes (control channel: no) | layer-4 collector | One `observe()` per crate |
| Access checks | `access`, `home`, voters | Logic yes, enforcement no | `access::check` | Security: enforcement only at owners |
| Control gate | `control`, `home` write path | Logic yes, enforcement no | `control` leaf | Correctness: gate and order are atomic |
| Calculations | `hub` sessions | Yes | `ctx` (kind) | Performance: placement near inputs |
| Secrets | `mesh` record, voters, node keys | Partly | typed record in `mesh`; sealing in `ops`; `ctx.secret` | One record type in `mesh` |
| Upgrades | `mesh` lock and versions, `blob`, `node`, format flag | Partly | `ops` rollout on public `mesh` calls; injected format flag | Version field on lease renewals |
| Discover | the kind, `ops` routing | Yes | `Kind::discover` | Endpoint busy while a run holds it |
| Plan and apply | `config`, `mesh` compare-and-swap, `blob`, voters | Partly (plan: yes) | `mesh::propose(Apply)` | None: the commit is the core's job |

---

## Part C. Decisions for the user

**R12-1. Split dependencies between `&self` and `ctx`.** The kind value holds
process-lifetime parts that `node` injects (registries, dialers, vendor libraries); `ctx`
holds one run's capabilities. Recommend yes.

**R12-2. The `ctx` capability set.** The list in A.4, and nothing more: scoped sessions,
status, run commands, secrets, cancel, mesh time, monotonic clock, random source, pooled
blocks, and dedicated threads. Recommend yes.

**R12-3. One handler per error class.** `Retry` at the composition's tick loop, `Device`
at `compose::groups`, `Config` at the supervisor, and any other error that leaves `run`
restarts it with backoff. Recommend yes. Alternative: the supervisor handles every class,
which forces a whole-run restart for a single bad group (the NI and LabJack cases).

**R12-4. A spec change restarts `run`.** No hot-reconfigure API in v1. Re-index rides on
the restart plus the seal order in `home`. Recommend yes. Trade: each apply that touches a
connector stops its device briefly and leaves a gap.

**R12-5. Connectors may share one physical medium through the kind's registry.** RS-485
multi-drop, a CAN bus, or one OPC UA server session can serve several connectors;
different settings on one key are a config error. Recommend yes. Alternative: one
connector per medium, which is simpler but makes one connector stand for many devices.

**R12-6. Compositions are plain async functions with closures.** No hook traits. A kind
that does not fit copies one. Recommend yes.

**R12-7. Vendor code runs only in `thread::Dedicated`, and threads are never detached.** A
stuck vendor call blocks only the next run of that connector and shows "stuck" status.
Recommend yes. Trade: a stuck call holds one thread until the process restarts, which is
visible instead of leaked.

**R12-8. Choose the shard by connector.** The run and every index it writes share one
shard. Recommend yes. Trade: an index cannot move shards alone; it moves with its
connector.

**R12-9. Status collection moves to layer 4 (I1).** Recommend yes.

**R12-10. Upward flow only through pulled values (I2).** Recommend yes, enforced by the
architecture check.

**R12-11. Time sources are injected `time::Source` values in layer 2, not connectors
(I3).** Recommend yes.

**R12-12. Standby seating: a separate copy component, takeover inside `mesh` and `home`.**
Recommend yes, with the scheme left to r13.

**R12-13. No always-on scan loop.** Discover is on demand, and health is the running
connector's status. Recommend yes.

**R12-14. Cyclic buses: one engine per connector, registrations fixed per run, period
from config.** Recommend yes. Trade: adding a slave or a group restarts the cycle once
per apply.

---

## Sources

Synnax (local repo, 2026-10-04): every `driver/`, `x/cpp/`, and `arc/cpp/` path cited
above; commits 113541d706, f7b08b6a75, 90613a3b0a, 05d3b8a0c6, b0612a81ec, 4c7ab913a2,
cfb402d224, c4b9b8e4f6, a25b369ba5, 2fe09c703e, 0762038a1f, 7a4e252a80, f599d8faea;
[unmerged] `driver/bus/{registry,connection,read}.h` on
`sy-4972-add-arinc-429-and-mil-std-1553-integrations-and-move-modbus`.

Prior art:

- Neil Brown, "Linux kernel design patterns, part 3", LWN, 2009:
  https://lwn.net/Articles/336262/
- Tomas Petricek, "Library patterns: Why frameworks are evil", 2015:
  https://tomasp.net/blog/2015/library-frameworks/
- OpenTelemetry Collector `component` and `scraperhelper`:
  https://pkg.go.dev/go.opentelemetry.io/collector/component,
  https://pkg.go.dev/go.opentelemetry.io/collector/scraper/scraperhelper
- Telegraf `input.go` and `docs/INPUTS.md`: https://github.com/influxdata/telegraf
- Kafka Connect `SourceTask` Javadoc:
  https://kafka.apache.org/documentation/#connect_developing; KIP-618:
  https://cwiki.apache.org/confluence/display/KAFKA/KIP-618%3A+Exactly-Once+Support+for+Source+Connectors
- Debezium `ChangeEventQueue`:
  https://github.com/debezium/debezium/blob/main/debezium-connector-common/src/main/java/io/debezium/connector/base/ChangeEventQueue.java
- Vector: `src/config/source.rs`, `lib/vector-core/src/source.rs`,
  `src/sources/util/http_client.rs` at https://github.com/vectordotdev/vector
- Redpanda Connect: `public/service/input.go` at https://github.com/redpanda-data/benthos;
  `internal/impl/mqtt/input.go` at https://github.com/redpanda-data/connect
- tower: https://docs.rs/tower-service/latest/tower_service/trait.Service.html,
  https://docs.rs/tower/latest/tower/trait.Layer.html
- embedded-hal SPI: https://docs.rs/embedded-hal/latest/embedded_hal/spi/index.html;
  embedded-hal-bus: https://docs.rs/embedded-hal-bus/latest/embedded_hal_bus/spi/index.html
- ROS 2 executors:
  https://github.com/ros2/ros2_documentation/blob/rolling/source/ROS-Framework/client-libraries/About-Executors/About-Executors.rst
- ros2_control: `hardware_interface/include/hardware_interface/hardware_component_interface.hpp`
  at https://github.com/ros-controls/ros2_control; asynchronous components:
  https://control.ros.org/rolling/doc/ros2_control/hardware_interface/doc/asynchronous_components.html
- EPICS asyn: https://epics-modules.github.io/asyn/asynDriver.html
- Ignition SDK example: `opc-ua-device` at
  https://github.com/inductiveautomation/ignition-sdk-examples; forum write-up:
  https://industrialmonitordirect.com/blogs/knowledgebase/handling-opc-ua-tag-subscriptions-in-ignition-sdk
- Tokio `sleep` ("operates at millisecond granularity"; Windows timers can be coarser):
  https://docs.rs/tokio/latest/tokio/time/fn.sleep.html. This is why `pace::Timer::wait`
  on a dedicated thread exists beside the async `tick`.
