- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. `clock::Clock`: monotonic time as
  `types::time::Monotonic`, and a `Sleep` future that resets without an allocation.
  `wall::Wall`: the OS wall clock, which only `clock` reads (a lint).
  `entropy::Entropy`: random bytes from the OS, or from the run's seed in simulation.
  `rng::Rng` is concrete (xoshiro256++ seeded from `Entropy`), so simulation replays it.
  `shards::Shards`, held only by `node`: the core count, and one thread per shard with
  its own executor. A shard's core is an index below the count, never an OS CPU
  number. `Shards::start` panics past the count: only `node` picks cores, so a bad
  index is a bug. `os` maps index `i` to the `i`-th CPU of its affinity set, which it
  reads once, so the count never changes (#116).
  `tasks::Tasks`: spawns `!Send` tasks on the current shard.
  `threads::Threads`: dedicated threads for blocking code. Each runs one future, and it
  waits for an event only by awaiting a future, so simulation controls every wait. A
  lint denies the std blocking waits (`park`, `Condvar`, `Barrier`, `mpsc` receive).
  `thread::Handle` and `thread::Error`, which `Shards` and `Threads` both return (#129).
  A `thread::Error` comes from a start, and a `thread::Panicked` from a join (#153).
  When a shard's main future completes, the shard drops its other tasks. A panic in any
  task ends its shard, and its `Handle::join` returns `thread::Panicked`. A dropped
  `Handle` would leave its thread running, so it is `#[must_use]`. On `os`, a shard is a
  Tokio `LocalRuntime` and `spawn_local` runs `Tasks`; on `sim`, the deterministic
  scheduler runs them. No other crate calls Tokio's timers or spawn. This changes "A
  panic in any task ends its shard" (https://github.com/synnaxlabs/foundation/pull/18)
  for the cases below (2026-10-08, #871). As anywhere in Rust, a panic that unwinds into
  the unwind of another panic aborts the process. On `os`, a panic in the poll or the
  drop of a task that code spawns with `tokio::spawn` on a shard or a dedicated thread,
  or with `tokio::task::spawn_local` on a shard, not through `Tasks`, does not end it:
  Tokio catches the panic. Tokio drops such a task during the unwind of a panic in its
  poll, so a panic that unwinds out of that drop aborts the process. A panic in the drop
  of the payload of a panic can escape Tokio's catches and end the shard or the thread.
  A release build aborts at any panic. Of the Foundation crates that `node` links, only
  `os` depends on Tokio, so only vendor code can spawn such a task. Lost: Tokio's
  `unhandled_panic` setting. It needs `--cfg tokio_unstable` in every build, and a
  `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` value, as the loom job and the cfg runs of
  `cargo xtask` set, replaces the flags of `.cargo/config.toml`. Decided by
  `laptop.architect-2` (2026-10-08T23:08:19Z, #871,
  https://github.com/synnaxlabs/foundation/issues/871#issuecomment-6070831264),
  approved by `laptop.architect` (2026-10-09T00:26:41Z, #2033,
  https://github.com/synnaxlabs/foundation/pull/2033#issuecomment-6071732611).
  `env::files` (#37) gives files under one data directory, with owned blocks and a
  sync that poisons the file on failure (S4). One handle at a time holds a file open
  to write, until it drops and its calls end; another write open fails with `Busy`
  (#392).
  `File::close` ends after the calls of its handle end; a drop closes without a wait
  (#516). Each `os` platform picks its own mechanism (#121). `env::net` (#44) gives UDP
  sockets that move GSO and GRO batches with ECN and the local address, TCP streams, and
  listeners. `env::serial` (#431) gives serial ports that move bytes at the line rate,
  with 8 data bits, a parity, and stop bits. Framing belongs to the protocol: a USB
  adapter hides the gap between frames, so a seam that split frames would act
  differently on `os` and `sim`. A socket, listener, or port may move to another thread
  before its first poll. The first poll binds it to its thread, and a poll on another
  thread panics. Amended (2026-10-08, #120): on `os`, a TCP stream or listener is a
  non-blocking socket that no reactor holds until its first poll, which registers it
  with the I/O driver of the Tokio runtime of that thread. `os::shards()` and
  `os::threads()` build their runtimes with `enable_io`. A connect uses the driver of
  the thread that polls it, and an accept that of its listener; each gives the stream
  back unregistered, so a shard can take a stream that another thread accepted (ONE
  PORT PER NODE). A first poll on a thread with no runtime or no I/O driver panics.
  Rejected: one I/O thread for every socket, as `os::files` uses; each message would
  cross a thread (C2 puts a parked wake at 4 to 9 us), and every socket would wait
  behind one thread. Socket options come from `rustix`, and `TCP_NOTSENT_LOWAT`, which
  it lacks, from one `libc::setsockopt`. Until #119 lands, `os::net()` is behind the
  cargo feature `net`, and its `udp` panics ("os::net has no UDP driver yet"); #119
  removes the feature and the panic. Decided by `laptop.architect-2` (2026-10-08
  02:32 UTC, #120,
  https://github.com/synnaxlabs/foundation/issues/120#issuecomment-6050971843). On
  `os`, a peer that resets after the handshake gives `Ok` from `Net::connect`, and the
  stream reads `Reset`. The kernel then holds no peer, so `Tcp::peer` is the remote of
  the connect, an IPv4-mapped address as plain IPv4, and any other address as given,
  with its scope and flow label. A caller that needs the kernel's peer there makes an
  interface change to `env::net`. Decided by `laptop.architect-2` (2026-10-08 15:42 UTC,
  #1789, https://github.com/synnaxlabs/foundation/pull/1789#issuecomment-6063559667).
  Amended (2026-10-07, #995): `env::net` also gives name lookups.
  `Net::resolve` gives an IP literal, also an IPv6 address in brackets, with no
  lookup, and keeps no cache. `NotFound` is final; `Io` is a failed lookup that a
  retry may fix, and a caller matches the variant, not the code. On `os`,
  `getaddrinfo` maps `EAI_NONAME` and `EAI_NODATA` to `NotFound`, `EAI_SYSTEM` to
  `Io` with `errno`, `EAI_AGAIN` to `Io` with `EAGAIN`, `EAI_MEMORY` to `Io` with
  `ENOMEM`, and each other code to `Io` with `EIO` (#1095). Decided by the
  architect, #995
  (https://github.com/synnaxlabs/foundation/issues/995#issuecomment-6030922608). A
  host with a NUL byte is `NotFound`, with no lookup. Decided by `laptop.architect-2`
  (2026-10-08 16:52 UTC, #1095,
  https://github.com/synnaxlabs/foundation/issues/1095#issuecomment-6064802287).
  `EAI_NONAME` with errno `EMFILE` or `ENFILE` is `Io` with that code: the lookup met
  a full descriptor table, so glibc ran it without part of its configuration or
  without its name service modules, and its answer is not final. Lost: `NotFound`
  for each `EAI_NONAME`, because it gives a wrong final answer while the table is
  full. Decided by `laptop.architect-2` (2026-10-08 17:19 UTC, #1919,
  https://github.com/synnaxlabs/foundation/pull/1919#issuecomment-6065269037). Each
  lookup runs `getaddrinfo` on an OS thread of its own, which ends with the lookup,
  also after its future drops, and reads errno right after the call. A spawn that
  fails gives `Io` with its errno. Nothing caps the threads in flight. The caller on
  record, `connector::http`, makes one lookup for each new connection, so the threads
  in flight are at most the new connections per second times about 30 s, while no
  name server answers. Add a cap before a caller can start lookups at a rate that
  grows with peers, devices, or data.
  Lost: Tokio's blocking pool, because the drop of a runtime waits for each of its
  blocking tasks, and `os` drops each runtime it builds. The literal grammar waits on
  #1927. Decided by `laptop.architect-2` (2026-10-08 17:08 UTC, #1919,
  https://github.com/synnaxlabs/foundation/pull/1919#issuecomment-6065081738).
  From the review of #1018: the bracketed IPv6 literal, and what `NotFound` and `Io`
  mean to a caller. Amended (2026-10-07, #1117): `Mode::Create` makes a missing file
  with `len` zeroed bytes. It treats an empty file that is there as missing and
  allocates it, because a crash between the create and the allocation leaves one. It
  opens any other file that is there as it is. A create that gives `Full` leaves no
  file at the path and keeps no blocks. Another error can leave an empty file at the
  path, as a crash can. `os` and `sim` both do this. Lost: an atomic create through a
  temporary name and a rename, so that the path never shows an empty file; the
  temporary file would show in `list` and need a sweep after a crash. Decided by the
  architect, #1117
  (https://github.com/synnaxlabs/foundation/issues/1117#issuecomment-6031488357).
  Amended (2026-10-07, #1112): on `os`, a write open can lock a new empty file before
  its create does. The create gives `Busy`, the empty file stays, and the next create
  allocates it. A caller that opens with `Create` only never meets it. Lost: Linux
  `O_TMPFILE` with `linkat`; macOS has no equivalent, so the two platforms would
  differ in this rule. Decided by the architect, #1112
  (https://github.com/synnaxlabs/foundation/pull/1112#issuecomment-6031672142). The
  text of the failure rule: the architect, #1117
  (https://github.com/synnaxlabs/foundation/issues/1117#issuecomment-6031721563).
  Text of the failure rule amended by the architect, #1112
  (https://github.com/synnaxlabs/foundation/pull/1112#issuecomment-6032450864): only
  `Full` promises no file; a flock, stat, or name check error after `openat` can leave
  the empty file that the create made. Lost: a promise that any failed create leaves no
  file it made.
  Amended (2026-10-07, #1310): a call of `Files` whose future drops can still run. A
  remove left so removes what the path names when it ends. Count the room of a
  removed file as used until `sync_dir` on its directory ends, and while a handle holds
  the file (#1301). Decided by `laptop.architect-2`, #1310, 2026-10-07T14:55:45Z
  (https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6035200491. The
  sentence on a dropped call: `laptop.architect-2`, 2026-10-07T16:23:52Z
  (https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6042117395).
  Supersedes the drop sentence of
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245. Lost:
  "a drop does not stop the remove", which `os` breaks when its I/O queue is full.
  Amended (2026-10-07, #1264): a crash before a `Mode::Create` open ends can leave the
  file that it makes with no bytes, and `Create` makes a file with no bytes `len` zeroed
  bytes. `sim` makes that file at a crash. Lost: an atomic create in `os` through a
  temporary name and a rename; it leaves a temporary file after a crash, which needs a
  sweep, and a caller already learns from its own header whether a file holds data.
  Decided by `laptop.architect-2` (2026-10-07T18:31:29Z):
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692.
  Amended (2026-10-07, #1604): `File::remove(self)` removes the file of a write handle,
  then closes the handle as `File::close`. It removes the file of the handle, by device
  and inode with no follow of a link, as FILE RENAME does: `NotFound { path }` when the
  path no longer names it, and nothing is removed. Until the remove ends, also after a
  drop of its future, a write open of the path gives `Busy`; on `os` the descriptor
  closes after the unlink, so the lock holds across processes until then. The race
  sentence of FILE RENAME holds for it too. A drop of the future can stop the remove
  before it starts, as for `Files::remove`; the file then stays, and the handle closes.
  The removal is not durable until `sync_dir` on its directory ends. A poisoned handle
  gives `Poisoned` and closes: a dropped rename can still move the file, so the path of
  the handle may be stale. A read handle panics. The caller is `mesh::log` (#1314),
  which removes a file with no record and later makes one at its path (MESH LOG). Lost:
  a spare name in `mesh` only, which adds a second kind of file to the directory of a
  log, a sweep of it in `Log::open`, and a change to the `Stray` rule of MESH LOG.
  Supersedes the "Lost: `File::remove`" sentence of
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245. Decided
  by `laptop.architect-2`, #1604, 2026-10-07T20:24:12Z
  (https://github.com/synnaxlabs/foundation/issues/1604#issuecomment-6046168932). The
  caller sentence: `laptop.architect`, 2026-10-08T02:36:07Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6051016964). The
  sentence that a drop can stop the remove: `laptop.architect-2`, 2026-10-08T02:32:41Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6050977855). The
  `Poisoned` sentence: `laptop.architect-2`, 2026-10-08T02:41:37Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6051075955).
