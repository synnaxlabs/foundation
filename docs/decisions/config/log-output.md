- **LOG OUTPUT (#2606, 2026-10-15)** A node writes its log to stderr, one line for each
  `tracing` event at the level of `--log-level` or above.
  (1) Flags of `foundation start`: `--log-level error|warn|info|debug|trace`, default
  `info`, lower case only, and `--log-format text|json`, whose default is `json` with
  `--json`, else `text`. A bad value is `ops.argument`, whose message ends with
  `` `<value>` is not one of <the values>``. No `off`: send stderr to `/dev/null`.
  `--json` stays the format of the command's own lines. The person approved structured
  events with a text and a JSON output and a level flag: "Ok, this is fine"
  (2026-10-13T21:06:00Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155197898). The
  details by `laptop.architect-2` (item 3 of 2026-10-13T21:13:43Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155317582).
  (2) The stamp of a line is the OS wall time of the node, in RFC 3339 UTC with six
  fraction digits, floored. Mesh time is the field `mesh_time`: absent for
  `clock::log::Mesh::Near`, the best guess in the stamp's form for `Apart`, and `none`
  (JSON `null`) for `Unsynced` (CLOCK LOG gives the cases and the 2 ms). The person:
  "Yeah I agree. System clock first" (2026-10-13T21:17:06Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155377794). The
  field by `laptop.architect-2` (2026-10-13T21:25:00Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155519701), amended
  by 2026-10-14T04:35:24Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6160981810.
  (3) Text: `<stamp> <LEVEL> <target>: <message> <key>=<value>... [mesh_time=...]`,
  with `mesh_time` last. The message has `\` and each control character escaped as
  `char::escape_debug` gives them. A string value that is empty, or holds a space, `=`,
  `"`, `\`, or a control character, is in double quotes, with `"`, `\`, and each
  control character escaped; each other value is plain, as logfmt writes it. So a value
  reads back to one string and cannot look like a second field. Integers and booleans
  are plain. JSON: one object, `{"timestamp", "level", "target", "fields":
  {"message", ...}, "mesh_time"}`, in that order, with the fields in event order,
  integers and booleans as JSON numbers and booleans, and each other value a string.
  By `laptop.architect-2` (items 2 and 3 of 2026-10-15T20:27:52Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6201604736).
  (4) `node::log` builds the `fmt` layer of `tracing-subscriber` with a `FormatEvent`
  of its own, which reads `clock::log::Reader::now` once for each line and writes the
  whole line, with `log_internal_errors(false)`, so a failed write drops the line and
  `fmt` writes nothing of its own to stderr. A `LevelFilter` of `--log-level` is under
  it. `fmt` keeps the buffer of each thread, the one write of each line, and the span
  fields that a later step needs. Features `fmt`, `registry`, and `std` only: no
  `ansi`, `json`, or `tracing-serde`. By `laptop.architect-2` (2026-10-15T20:28:52Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6201622804).
  Lost: a `Layer` of our own, which builds again what `fmt` gives; the `json` format
  of `fmt`, which needs a second clock read or a splice for `mesh_time`. The writer
  of `fmt` puts each line in the queue of item 9, so no event waits on stderr.
  (5) No dispatch is global (crate map rule 9, `clippy.toml`). `Node::start` makes the
  dispatch from `Config::log` and gives it to its shards through `Shards::logged`.
  `node::Log` holds the writer and the `ops::Log` that `start` parses, whose default
  is text at `info`. By `laptop.architect-2` (2026-10-15T21:40:21Z,
  https://github.com/synnaxlabs/foundation/pull/2762#issuecomment-6202853376).
  Shard 0 writes `started` (INFO, `name`, `address`: the bound address, `shards`) once
  the file `address` holds the address. `Node::join` writes `stopped`: INFO after a
  stop, ERROR with `error` after a failure, also a failure before any shard started.
  (6) The fix of `node.session` does not point to the log, which holds no event of a
  failed session yet (OPS OUTPUT, #2767). By `laptop.architect-2`
  (2026-10-15T22:05:27Z,
  https://github.com/synnaxlabs/foundation/pull/2762#issuecomment-6203245390).
  (7) The thread `log` (item 9) starts from `node::Config::threads` with
  `Threads::logged` and no dispatch, so the events of that thread go nowhere. No other
  thread starts from `Config::threads` yet; the first one gets the dispatch of the node
  with `Threads::logged`. The threads of `main` (`show`, `stop`, the I/O thread of each
  disk) get the dispatch with the first event one of them writes. By
  `laptop.architect-2` (2026-10-15T20:38:04Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6201785053), with
  the thread of #2764.
  (8) P1 holds at `info`. With each write on the thread of its event, the log cost about
  1.1 µs for each run of a node and 33 KB idle, and #2764 adds the costs below. A level
  under `info` is an operator's choice for debug, and its cost is outside P1. At `debug`
  and `trace`, `noq-proto` writes a line for each packet and frame of QUIC, at about 425
  to 739 ns and 2 to 6 allocations before the write, and at `trace` a span at 118 ns and
  2 allocations (laptop, M3 Max, load 9, 3 passes). The help of `--log-level` says so.
  #2764 moves each write to the thread `log`, so an event holds the lock of the queue
  only for its push. That adds about 0.4 µs to a line in a burst, about 1 µs to a line
  that wakes the thread, about 23 µs to each start of the thread, and about 90 KiB to an
  idle log (laptop, M3 Max, load 12 to 17, 6 passes of each in turns, at `3b29bcf77`,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6223376242), under P1.
  With the thread on the CPU of the event, a line adds 0.07 to 0.09 µs and a start 14 µs
  (AWS c7i.metal-24xl, quiet host,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6221594837). #2913
  measures it on Linux with the thread on its own CPU. #2815 carries the allocations and
  the span fields that `Line` never prints. #2807 carries the WARN of `noq-proto` for
  each Initial packet with an invalid retry token. By `laptop.architect-2`
  (2026-10-16T01:49:38Z,
  https://github.com/synnaxlabs/foundation/pull/2762#issuecomment-6206195113), and the
  split to #2807 (2026-10-16T03:22:23Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6207344849).
  (9) The thread `log` writes each line, so no event waits on the writer. An event puts
  its line in a queue of 1024 lines and 1 MiB, and wakes the thread. The lines, the
  count of dropped lines, the panic lines, the waker, and the run of the thread and its
  handle are under one lock, so each line, each drop, and each close wakes the thread,
  and a line that comes as the thread ends gets a thread that writes it; loom models
  check both. The thread takes the lines out under the lock and writes them with the
  lock free, and each line is formatted before its push, so a writer that never returns
  holds no lock. Nothing that can panic runs under the lock: the panic hook takes the
  same lock. A line drops when 1024 lines wait or when it does not fit in 1 MiB with
  them, and each line after it drops until the thread takes the queue. The thread writes
  WARN `dropped` (target `node::log`) with the field `lines`, their count, after the
  last line that came before the first of them. At λ lines a second, 1024 lines hold a
  stall of the writer of 1024/λ s: 10 ms at 100,000 lines a second, a rate only `debug`
  and `trace` reach (item 8). The thread writes at most one batch beside the lines that
  wait, so the log holds at most 2048 lines and 2 MiB, beside the panic lines. The count
  line reports each loss. The depth of the queue gets a gauge when the node has metrics.
  A write that fails drops its line. A write that panics ends the thread `log`, and no
  later line reaches the writer; a release build, with `panic = "abort"`, ends there.
  The thread starts with the node, which fails with `Error::Start` when it cannot. It
  ends once each shard has ended and the queue is empty, so a sim run ends. A line after
  that waits in the queue for `Node::join` (after `stopped`) or `Flush::wait`. Each
  starts the thread again through one private resume, which starts it only when none
  runs and a line or the count of a drop waits, so at most one thread `log` runs and the
  lines keep their order. Inside a `tracing` event, no OS thread can start and no
  dispatch can be set. A line from another thread after the last `Flush::wait` of `main`
  is lost. `main` waits up to 1 s for the log at its end. `main` alone sets the panic
  hook, to `Flush::hook` over the hook before it. The hook puts ERROR `panicked` with
  the field `panic` in the queue after each line there, also when the queue is full:
  this line never drops. It sets no dispatch and starts no thread, since a panic can
  come from inside an event. While a thread `log` runs on another thread, the hook
  waits up to 1 s for the log to write each line. It calls the hook before it unless
  that bound passes while that thread still runs, since the writer may block that write
  too (laptop.architect-2, 2026-10-17T08:31:40Z,
  https://github.com/synnaxlabs/foundation/pull/3024#issuecomment-6231774507). So when
  no thread `log` runs, as after each shard has ended and the thread wrote each line, it
  calls the hook before it at once, and the line waits for the next `Flush::wait`
  (laptop.architect-2,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6209084526). A panic
  of the writer, on the thread `log`, also calls it at once, since only that thread
  writes the line.
  Decision by `laptop.monitor` (2026-10-15T21:39:25Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6202838819). The
  plan by `laptop.architect-2` (2026-10-15T21:43:54Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6202910326), and its
  change (2026-10-16T03:22:23Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6207344849), and the
  panic hook with no thread start (2026-10-16T03:48:16Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6207638284), and no
  wait on the thread `log` (2026-10-16T04:04:09Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6207813688), and
  `Flush::hook` (2026-10-16T04:37:28Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6208190479). The
  hook in `main` only, by `laptop.director` (2026-10-15T21:38:54Z,
  https://github.com/synnaxlabs/foundation/issues/2764#issuecomment-6202830785), and a
  child test whose hook before it aborts in place of a test of the release binary
  (2026-10-16T05:53:41Z,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6209079754). The 1 MiB
  cap, the gauge when the node has metrics, the count under the one lock, the thread
  that ends and starts again, and the name `Flush`, by `laptop.architect-2`
  (2026-10-16T05:54:05Z,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6209084526). Each line
  after a drop drops until the thread takes the queue, and the count of a drop starts
  the thread, by `laptop.architect-2` (2026-10-16T06:52:19Z,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6209868324). The bound
  of 2048 lines and 2 MiB with the batch, the loom model of the handoff, and one end of
  a run (2026-10-16T07:05:11Z,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6210050516). The lock
  queue as the mechanism of the queue, with its two rules, by `laptop.architect-2`
  (2026-10-16T17:46:37Z,
  https://github.com/synnaxlabs/foundation/pull/2812#issuecomment-6221353661). Lost: one
  thread for the life of the log, which makes each sim run end as `Stuck`; a write on
  the thread of `join`, which loses the bound of `main` on a stuck writer;
  `tracing-appender::non_blocking`, which starts its own OS thread, so no sim run drives
  it, and counts a dropped line with no line in the log; an atomic count, a second sync
  point beside the lock that each push takes; a bounded `sync_channel` with an atomic
  count of drops: the byte cap, the panic line past the bound, the drop until the take,
  and the end of a run in the take each need one step over the queue and the counts,
  which a second lock beside the channel would give, at the cost of two locks to order.
