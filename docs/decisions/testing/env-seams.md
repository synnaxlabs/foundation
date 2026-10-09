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
  scheduler runs them. No other crate calls Tokio's timers or spawn. `env::files`
  (#37) gives files under one data directory, with owned blocks and a sync that
  poisons the file on failure (S4). One handle at a time holds a file open to write,
  until it drops and its calls end; another write open fails with `Busy` (#392).
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
  gives `Unreachable`. A `Transmit` goes out in one `sendmsg`, with GSO; there is no
  `sendmmsg`. After `EIO` or `EINVAL` on a GSO send, `noq-udp` stores 1 as its
  `max_gso_segments`, and from then on each datagram goes out alone; that is the only
  GSO flag. Each half has its own `dup` of the socket. The receiver registers for
  readable at its first poll, in a `OnceLock`, so no lock is on the receive path. A
  sender registers for writable at its first poll and after each `EAGAIN`, and the next
  send that succeeds drops the registration: Linux wakes each `EPOLLOUT` registration of
  a socket for each datagram that the socket sends (1,000 wakes for 1,000 sends on
  box2), so a sender that stays registered on each shard would wake each parked shard.
  Decided by `laptop.architect-2` (2026-10-08 18:27 UTC, #119,
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541).
  The first poll of a UDP half binds it to its thread, whatever its result. A failed
  `dup` or registration gives `Io` for that poll alone, and the next poll tries again;
  nothing stores a failure. For a source that is not local or is of the other family,
  `os` gives the kernel's answer (on Linux, `Unreachable` or `Io { code: 22 }`), and
  `sim` gives `Io { code: 99 }`. For port 0, `os` gives the kernel's answer. Until
  #1972 patches `noq-udp`, such a transmit can turn GSO and the IPv4 ECN mark off for
  the life of the socket. Decided by `laptop.architect-2` (2026-10-08 18:56 and 19:02
  UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066909518,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6067014743).
  Supersedes "or the errno" of item 2 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541: a
  failure is not stored. One exception: `os` gives `Io { code: 22 }` for each IPv6
  source on an IPv4 socket, mapped too, or an unspecified source in any form. Linux
  skips the `IPV6_PKTINFO` of the first and reads the second as no source, and sends
  each from an address of its choice. Decided by `laptop.architect-2` (2026-10-08 20:06
  and 20:38 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068090235,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068606545).
  Supersedes https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6068520601,
  which refused only `0.0.0.0`. On macOS, `os` has no GSO, so `batch_max` is 1, and the
  loopback, with an MTU of 16,384 bytes, loses a larger datagram. Decided by
  `laptop.architect-2` (2026-10-08 22:35 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6070425767). Until
  #1972 patches noq-udp to send an IPv4 source as `IP_PKTINFO` on Apple, macOS ignores
  each IPv4 source and sends from an address of its choice, with no error. Decided by
  `laptop.architect-2` (2026-10-08 22:51 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6070627958).
  `os::net()` is behind the `os` cargo feature `net`, off by default, because Tokio has
  no `net` under `--cfg loom`. Decided by `laptop.architect-2` (2026-10-08 23:42 UTC,
  #1965, https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6071230415).
  Supersedes the removal of the feature in
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066303437, approved
  in https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066705829. Trigger:
  #2038 gives the loom models a cfg name of their own, and then removes the feature. On
  `os`,
  a peer that resets after the handshake gives `Ok` from `Net::connect`, and the stream
  reads `Reset`. The kernel then holds no peer, so `Tcp::peer` is the remote of the
  connect, an IPv4-mapped address as plain IPv4, and any other address as given, with
  its scope and flow label. A caller that needs the kernel's peer there makes an
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
