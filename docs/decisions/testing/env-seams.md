- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. Amended (2026-10-09, #2063): a driver of
  `tasks` that counts tasks passes each task to one of theirs, and a driver of a test or
  a benchmark may keep each task for its caller to poll (`laptop.architect-2`,
  2026-10-09T03:42:09Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6073808019).
  Supersedes the ENV SEAMS and `Driver` texts of item 4 of
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072358016.
  `clock::Clock`: monotonic time as `types::time::Monotonic`, and a `Sleep` future that
  resets without an allocation.
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
  approved by `laptop.architect` (2026-10-09T01:09:48Z, #2033,
  https://github.com/synnaxlabs/foundation/pull/2033#issuecomment-6072192755).
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
  it lacks, from one `libc::setsockopt`. Decided by `laptop.architect-2` (2026-10-08
  02:32 UTC, #120,
  https://github.com/synnaxlabs/foundation/issues/120#issuecomment-6050971843). On
  `os`, a UDP socket uses `noq-udp` for its socket calls: GSO, GRO, `recvmmsg`, ECN,
  the local address, and don't-fragment. `os` binds with `rustix`, with `IPV6_V6ONLY`
  off on an IPv6 socket, and routes as `sim` does: a socket on `::` sends IPv4 as
  `::ffff:a.b.c.d`, and any other socket that gets a destination of the other family
  gives `Unreachable`. A `Transmit` goes out in one `sendmsg`, with GSO, or one per
  datagram after the kernel refuses GSO; there is no `sendmmsg`. After `EIO` or
  `EINVAL` on a GSO send, `noq-udp` sends the first datagram alone. Only when it goes
  out does `noq-udp` store 1 as its `max_gso_segments`, and from then on each datagram
  goes out alone; that is the only GSO flag (`laptop.architect-2`, 2026-10-08 21:15
  UTC, https://github.com/synnaxlabs/foundation/issues/1972#issuecomment-6069193130).
  Supersedes the store of 1 on `EIO` or `EINVAL` of item 1 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541. After
  `Pending` partway through a transmit sent one datagram at a time, the `os` sender
  keeps the index of the next datagram, so the retry with the same transmit sends only
  the datagrams that did not go out (`laptop.architect-2`, 2026-10-09 07:15 UTC,
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6076288463). A
  different transmit in place of the one that got `Pending` can lose its first
  datagrams, at most as many as went out before the `Pending` (`laptop.architect-2`,
  2026-10-09 13:03 UTC,
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6081443711). Each
  half has its own `dup` of the socket. The receiver registers for readable at its
  first poll, in a field of its driver (`laptop.architect-2`, 2026-10-08 19:12 UTC,
  https://github.com/synnaxlabs/foundation/issues/1974#issuecomment-6067190077). The
  `os` receiver's driver has no `Mutex` of its own (item 2 of `laptop.architect-2`,
  2026-10-08 18:27 UTC,
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541). Its
  poll and its drop lock only Tokio's `Mutex`es, in Tokio's `AsyncFd` and I/O driver.
  `laptop.architect-2` reads item 2 as no `Mutex` of our own, with Tokio's own locks
  allowed (2026-10-09 03:18 UTC,
  https://github.com/synnaxlabs/foundation/pull/2068#issuecomment-6073564290). The
  receiver's `receiver::Driver` is its alone, as a sender clone's is:
  `net::Driver::udp` gives it at the bind, beside the socket (`laptop.architect-2`,
  2026-10-09 01:42 UTC,
  https://github.com/synnaxlabs/foundation/pull/2068#issuecomment-6072529426).
  Supersedes the `OnceLock` of item 2 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541 and the
  two `OnceLock`s of item 3 of
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6067014743. A
  sender registers for writable at its first poll and after `EAGAIN`, and drops the
  registration when the send ends: Linux wakes each `EPOLLOUT` registration of a socket
  for each datagram that the socket sends (1,000 wakes for 1,000 sends on box2), so a
  sender that stays registered on each shard would wake each parked shard. Decided by
  `laptop.architect-2` (2026-10-08 18:27 UTC, #119,
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541). A send
  ends at its first `Ready`, also with an error or a failed wait for writable. A send
  that the caller drops while it waits keeps the registration until the next send of
  that sender ends. Supersedes "the next send that succeeds deregisters it" of item 3
  of https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541.
  Decided by `laptop.architect-2` (2026-10-09 00:34 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6071816983).
  The first poll of a UDP half binds it to its thread, whatever its result. A failed
  `dup` or registration gives `Io` for that poll alone, and the next poll tries again;
  nothing stores a failure. For a source that is not local or is of the other family,
  `os` gives the kernel's answer (on Linux, `Unreachable` or `Io { code: 22 }`), and
  `sim` gives `Io { code: 99 }`. For port 0, `os` gives the kernel's answer. Such a
  transmit changes no state of the socket (`laptop.architect-2`, 2026-10-08 21:15 UTC,
  https://github.com/synnaxlabs/foundation/issues/1972#issuecomment-6069193130).
  Decided by `laptop.architect-2` (2026-10-08 18:56 and 19:02 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066909518,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6067014743).
  Supersedes "or the errno" of item 2 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541: a
  failure is not stored. One exception: `os` gives `Io { code: 22 }` for each IPv6
  source on an IPv4 socket, mapped too, for an IPv6 source that is not mapped with an
  IPv4 destination, and for an unspecified source in any form. Linux skips the
  `IPV6_PKTINFO` of the first, macOS skips that of the second, and Linux reads the
  third as no source; each then sends from an address of its choice. Decided by
  `laptop.architect-2` (2026-10-08 20:06 and 20:38 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068090235,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068606545; the
  second case 2026-10-09 04:36 UTC, #2097,
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6074356047).
  Supersedes https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068520601,
  which refused only `0.0.0.0`. On macOS, `os` has no GSO, so `batch_max` is 1, and the
  loopback, with an MTU of 16,384 bytes, loses a larger datagram. Decided by
  `laptop.architect-2` (2026-10-08 22:35 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6070425767). On Apple,
  the `noq-udp` patch sends an IPv4 source to an IPv4 destination as `IP_PKTINFO`, and
  `os` refuses no IPv4 source beyond the exception above. Decided by
  `laptop.architect-2` (2026-10-08 22:51 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6070627958). This
  holds unless a caller turns on the fast path with
  `UdpSocketState::set_apple_fast_path`, which no crate here calls outside the copy's
  own test `apple_fast_datapath`. Its trigger is in the `noq-udp` row of "Local patches"
  in `docs/dependencies.md`. The limit: `laptop.architect-2` (2026-10-09 05:03 UTC,
  #2097, https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6074645099).
  The rule, the limit, and the pointer as they are now: `laptop.architect-2` (2026-10-09
  05:26 UTC, #2097,
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6074891760).
  Supersedes the `fast-apple-datapath` condition of
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6074544404
  (04:55 UTC).
  `os::net()` is behind the `os` cargo feature `net`, off by default, because Tokio has
  no `net` under `--cfg loom`. Decided by `laptop.architect-2` (2026-10-08 23:42 UTC,
  #1965, https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6071230415).
  Supersedes the removal of the feature in
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066303437, approved
  in https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066705829, and
  item 2 of https://github.com/synnaxlabs/foundation/issues/120#issuecomment-6050971843,
  which removed it when the last of #119 and #1095 merged. Trigger: #2038 gives the
  loom models a cfg name of their own, and then removes the feature. On `os`, a peer
  that resets after the handshake gives `Ok` from `Net::connect`, and the stream reads
  `Reset`. The kernel then holds no peer, so `Tcp::peer` is the remote of the connect,
  an IPv4-mapped address as plain IPv4, and any other address as given, with its scope
  and flow label. A caller that needs the kernel's peer there makes an interface
  change to `env::net`. Decided by `laptop.architect-2` (2026-10-08 15:42 UTC,
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
  Amended (2026-10-08T17:19:48Z, #1921): on macOS, an accepted socket does not keep
  the receive buffer of its listener, so `os` sets the options of the listener again
  on each accepted socket, on every OS. The listener still sets them before `listen`:
  the window scale of the SYN-ACK comes from its receive buffer. On macOS, a socket
  option on a socket that a reset ended gives `EINVAL`. On `ENOTCONN` or `EINVAL`
  from a call on the socket, `os` reads the pending error and gives it: a pending
  error ends the stream, and with none the call gives `Io` with the code. Decided by
  `laptop.architect-2` (2026-10-08T17:19:48Z:
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6065277473; the
  one rule for `ENOTCONN` or `EINVAL`, 2026-10-08T18:12:38Z:
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6066172138).
  Measured on macOS (#1921): a create of a path with a trailing slash gives `ENOTDIR`
  for a file and `NotFound` for no file, not `EISDIR`, and an unlink of a directory
  gives `EPERM`, not `EISDIR`. No code reads these codes.
  Amended (2026-10-08T17:46:08Z, #1921): macOS applies `TCP_NOTSENT_LOWAT` only to the
  write event, not to the write itself. So on macOS, `os` counts the bytes written since
  the count last reached the bound. When the count reaches `unsent_bytes_max`, the next
  write waits for the write event, which honors the bound. So the unsent bytes stay at
  most twice the bound, with one wait per `unsent_bytes_max` bytes. On macOS with
  `delayed`, XNU also posts the write event under one segment, so the unsent bytes stay
  at most the bound plus the larger of the bound and one segment. Decided by
  `laptop.architect-2` (2026-10-08T17:46:08Z:
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6065728469; the last
  two sentences, 2026-10-08T18:12:38Z:
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6066172138).
  Supersedes the second item of
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6065283346
  (2026-10-08T17:20:09Z), and the last sentence of the record text of
  https://github.com/synnaxlabs/foundation/issues/1921#issuecomment-6065728469
  (2026-10-08T17:46:08Z).
  Amended (2026-10-08T19:21:24Z, #1977): a write of no bytes gives `Ok(0)` at once, with
  no wait and no error, also after a reset or `poll_close`. It writes nothing, so it has
  nothing to report; the next write of bytes, read, or close reports a reset or a close.
  `os` and `sim` each return after the thread check (and, in `sim`, the crash check),
  before any other step, so the two agree with no shared logic. The thread check binds
  the stream to its thread, also at a first poll that writes no bytes. Lost: `Ok(0)` or
  the error that ended the stream, which needs a read of `SO_ERROR` in `os` and the
  close and reset states in both drivers, for a call that no caller makes; and a wait as
  for a write of bytes, which needs the kernel's unsent count on each such call and is
  not exact on macOS. Decided by `laptop.architect-2` (2026-10-08T19:21:24Z:
  https://github.com/synnaxlabs/foundation/issues/1977#issuecomment-6067346540; the
  order, 2026-10-08T19:44:46Z:
  https://github.com/synnaxlabs/foundation/pull/1938#issuecomment-6067736678).
  Amended (2026-10-08T17:39:11Z, #1940): `tcp::Options::unsent_bytes_max` is a
  `NonZeroUsize`. A bound of 0 has no meaning in its doc, and the drivers did not agree
  on it: Linux read it as the host sysctl `net.ipv4.tcp_notsent_lowat`, by default no
  bound, the macOS kernel read it as no bound, `os` on macOS wrote 1 byte per call, and
  `sim` never wrote. No caller gives 0. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1940). The Linux text:
  `laptop.director`, 2026-10-09T01:51:29Z
  (https://github.com/synnaxlabs/foundation/pull/2044#issuecomment-6072622027).
  Amended (2026-10-08T21:01:28Z, #2000): each socket that `os` opens is closed on exec.
  On Linux, the call that opens or accepts the socket sets that, so a child that
  another thread spawns holds it only from its fork to its exec (the window,
  `laptop.architect-2`, 2026-10-09T05:02:07Z:
  https://github.com/synnaxlabs/foundation/pull/2109#issuecomment-6074628714). macOS has
  no such flag, and std sets it in a second call on an accepted stream too, so on
  macOS that child may hold the socket and its port, as the doc of `os::net()` says. A
  Foundation node spawns no process, so only tests see it, and CI runs on Linux. Lost:
  `POSIX_SPAWN_CLOEXEC_DEFAULT` on the spawn side, which std does not set, and which
  needs a spawn seam and `unsafe` for tests only. Decided by `laptop.architect-2`
  (2026-10-08T21:01:28Z: https://github.com/synnaxlabs/foundation/issues/2000). On
  macOS, a child that another thread spawns during a lookup may also hold the sockets
  that the C library opens for it, and a child spawned after a lookup may hold a socket
  that the C library keeps open. `os` keeps this gap for the same reason. It cannot set
  the flags of the sockets of a lookup, because the C library opens them, and it cannot
  open them another way without its own resolver. Lost: a resolver in `os`, which
  changes what each lookup gives on macOS. Trigger: the first change that makes a node
  spawn a process closes each macOS gap of this amendment on the spawn side. Decided by
  `laptop.architect` (2026-10-09T02:37:32Z:
  https://github.com/synnaxlabs/foundation/pull/2072#issuecomment-6073119953; the socket
  that the C library keeps open, 2026-10-09T02:42:31Z:
  https://github.com/synnaxlabs/foundation/pull/2072#issuecomment-6073170413). Amended
  (2026-10-09, #2108): an `os` listener stops its listen at once on Linux when it drops
  or its registration fails, also while a child holds a copy of the socket, from its
  fork to its exec. macOS has no call that stops the listen of a copy, so there a
  connect in that window succeeds and resets at the exec (`laptop.architect-2`,
  2026-10-09T04:57:56Z:
  https://github.com/synnaxlabs/foundation/pull/2109#issuecomment-6074581669; the failed
  registration, 2026-10-09T05:02:07Z:
  https://github.com/synnaxlabs/foundation/pull/2109#issuecomment-6074628714; the
  registration that gives the socket back, 2026-10-09T05:25:55Z:
  https://github.com/synnaxlabs/foundation/pull/2109#issuecomment-6074885496).
