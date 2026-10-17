- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. Amended (2026-10-09, #2063): a driver of
  `tasks` that counts tasks passes each task to one of theirs, and a driver of a test or
  a benchmark may keep each task for its caller to poll (`laptop.architect-2`,
  2026-10-09T03:42:09Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6073808019).
  Supersedes the ENV SEAMS and `Driver` texts of item 4 of
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072358016.
  Amended (2026-10-10, #2064): `tasks::Group` is the one counting group of `env`. It
  counts each task spawned through the `Tasks` that `Group::tasks` gives until the
  task's future has dropped, and `Group::ended` gives an `Ended` that resolves only
  then, with many waiters. `Group` does not implement `Driver`. The `Debug` text of
  each is `Group { .. }` and `Ended { .. }`. `laptop.architect-2`
  (2026-10-09T21:02:50Z):
  https://github.com/synnaxlabs/foundation/issues/2064#issuecomment-6089208454, and
  `Group::tasks` in place of the `Driver` impl (`laptop.architect-2`,
  2026-10-10T03:02:51Z):
  https://github.com/synnaxlabs/foundation/pull/2218#issuecomment-6093107567, which
  supersedes `impl Driver for Group` in 6089208454. A crate
  that counts its tasks to wait until each has ended uses it, and keeps no count of its
  own. A future that gives the result of one task, such as `buffer::End`, counts
  nothing, so the rule does not cover it. `laptop.architect` (2026-10-10T03:02:58Z):
  https://github.com/synnaxlabs/foundation/pull/2218#issuecomment-6093108453.
  Supersedes "no change to `env`" in
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6067043337.
  Amended (2026-10-12, #2214): a driver of `tasks` does not count tasks to wait for
  them to end, and a part that waits for its tasks uses `Group`. A driver of a test may
  count the tasks it passes on, to check them. Supersedes "a driver of `tasks` that
  counts tasks passes each task to one of theirs" of the #2063 amendment
  (https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6073808019).
  `laptop.architect-2` (2026-10-12T23:44:03Z):
  https://github.com/synnaxlabs/foundation/pull/2496#issuecomment-6134761989.
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
  Amended (2026-10-13, #2606): `Shards::logged` and `Threads::logged` give a clone that
  starts each thread with a `tracing::Dispatch`, the log of its node. The driver runs
  each call of the thread's code with it as the default dispatch: the call of `main` or
  `body`, each poll, each drop of a task, of `main`, or of `body`, and each drop of a
  panic payload that the driver catches. A panic that `start` passes to its caller is
  the caller's. `os` sets it for the life of the OS thread and around the spawn, whose
  failure drops the node's code on the calling thread. `sim` enters it around each such
  call. Each waker of a net, serial, or file event wakes and drops with the log of the
  thread whose poll gave it, and so does the memory of its event. A waker given outside
  a poll keeps the default log where it is given: that of the thread in a drop of its
  code, else that of the caller. A `Shards` or `Threads` that is not logged drops each
  event (`laptop.architect-2`, 2026-10-13T21:13:43Z, item 1 of
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155317582; rule
  meaning: `laptop.director`, 2026-10-13T21:12:40Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6155299486; the
  drops of tasks, mains, bodies, and payloads, and the spawn on `os`:
  `laptop.architect-2`, 2026-10-14T03:14:18Z,
  https://github.com/synnaxlabs/foundation/pull/2657#issuecomment-6160057412, and
  2026-10-14T03:43:43Z,
  https://github.com/synnaxlabs/foundation/pull/2657#issuecomment-6160393075; the wakers
  of events: `laptop.architect-2`, 2026-10-14T03:50:33Z,
  https://github.com/synnaxlabs/foundation/pull/2657#issuecomment-6160469132; a waker
  given outside a poll: `laptop.architect-2`, 2026-10-14T04:19:55Z,
  https://github.com/synnaxlabs/foundation/pull/2657#issuecomment-6160804099).
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
  datagram after the kernel refuses GSO; there is no `sendmmsg`. A `Transmit` holds at
  most `TRANSMIT_BYTES_MAX`, so a GSO batch gets `EMSGSIZE` only when its segment is
  over the path MTU. Then each full segment is lost, and `os` sends the short last
  datagram alone (`laptop.architect-2`, 2026-10-09 22:36 UTC,
  https://github.com/synnaxlabs/foundation/pull/2193#issuecomment-6090406742).
  After `EIO` or `EINVAL` on a GSO send, `noq-udp` sends the first datagram alone. Only
  when it goes out does `noq-udp` store 1 as its `max_gso_segments`, and from then on
  each datagram goes out alone; that is the only GSO flag (`laptop.architect-2`,
  2026-10-08 21:15 UTC,
  https://github.com/synnaxlabs/foundation/issues/1972#issuecomment-6069193130).
  Supersedes the store of 1 on `EIO` or `EINVAL` of item 1 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541. After
  `Pending` partway through a transmit sent one datagram at a time, the `os` sender
  keeps the index of the next datagram, so the retry with the same transmit sends only
  the datagrams that did not go out (`laptop.architect-2`, 2026-10-09 07:15 UTC,
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6076288463). A
  transmit in flight when GSO turns off is the exception: its retry can send again
  datagrams that went out. A different transmit in place of the one that got
  `Pending` can lose its first datagrams (`laptop.architect-2`, 2026-10-09 13:18 UTC,
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6081691897).
  Supersedes the counts of
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6081443711
  (2026-10-09 13:03 UTC) and
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6081528612
  (2026-10-09 13:09 UTC), the count of
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6081197695
  (2026-10-09 12:48 UTC), and "at most once for each socket" of
  https://github.com/synnaxlabs/foundation/pull/2097#issuecomment-6076288463
  (2026-10-09 07:15 UTC) and
  https://github.com/synnaxlabs/foundation/issues/1972#issuecomment-6069193130
  (2026-10-08 21:15 UTC) (`laptop.architect-2`, 2026-10-09 13:43 UTC,
  https://github.com/synnaxlabs/foundation/pull/2142#issuecomment-6082138285).
  Each half has its own `dup` of the socket. The receiver registers for readable
  at its first poll, in a field of its driver (`laptop.architect-2`, 2026-10-08
  19:12 UTC,
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
  Changed (2026-10-16, #1927): a NUL byte is `host::Error::Character`, so it never
  reaches the driver. Decided by `laptop.architect-2` (2026-10-16 02:53 UTC, #1927,
  https://github.com/synnaxlabs/foundation/issues/1927#issuecomment-6206963417).
  Supersedes https://github.com/synnaxlabs/foundation/issues/1095#issuecomment-6064802287.
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
  blocking tasks, and `os` drops each runtime it builds. #1927 gives the grammar
  (below). Decided by `laptop.architect-2` (2026-10-08 17:08 UTC, #1919,
  https://github.com/synnaxlabs/foundation/pull/1919#issuecomment-6065081738).
  Amended (2026-10-16, #1927): `env` owns the whole grammar of a host.
  `Net::resolve` takes an `env::net::host::Host`, which a caller parses at plan for a
  config. An IPv4 address has four decimal parts with no leading zeros. An IPv6
  address is bare or in brackets, with an optional zone of a decimal interface number
  that fits in `u32`, which `resolve` gives as the scope with no lookup. A name has
  parts of ASCII letters, digits, `-`, and `_`, each 1 to 63 bytes, none that starts
  or ends with `-`. It is at most 253 bytes without one optional final dot, its last
  part is not a number, and its letters go to lowercase. Only a name reaches the
  driver, so `getaddrinfo` never reads an `inet_aton` form (`127.1`, `0x7f.0.0.1`,
  `10.0.0.2.`): `Host` refuses each. A NUL byte is `host::Error::Character`, not
  `NotFound`. A zone by interface name (`%eth0`) is refused, as it needs
  `if_nametoindex`: a new driver call, and interfaces in `sim`. Trigger: a user needs
  a link-local device by its interface name. Lost: `resolve(&str)` with a separate
  check for plan and a new error at run time, because the grammar runs twice and a
  caller can skip the check. Lost: a `host::Name` type for the driver, a public type
  for one invariant inside `env`: `Driver::resolve` keeps `&str`, and `os` fails
  loudly on a NUL. Proposed by `box1.builder-2`
  (https://github.com/synnaxlabs/foundation/issues/1927#issuecomment-6206949254).
  Decided by `laptop.architect-2` (2026-10-16 02:53 UTC, #1927,
  https://github.com/synnaxlabs/foundation/issues/1927#issuecomment-6206963417).
  Amended in the review of #2804: zone 0 is no zone, so one address has one `Host`.
  `host::uri` reads the host of a URI: an IPv6 address is in brackets, and its zone
  follows `%25` (RFC 6874), so `[fe80::1%252]` is zone 2 and another `%` is
  `Error::Escape`, whose message shows the `%25` form. Each caller that reads a URI uses
  it (R7 STARTING POINTS for `connector::http`; `connector-opcua` in #2803).
  `connector::http` parses the host again on each request, until #1218. Approved by
  `laptop.architect-2` (2026-10-16 04:05 UTC,
  https://github.com/synnaxlabs/foundation/pull/2804#issuecomment-6207830826; the move
  to `host::uri`, 04:34 UTC,
  https://github.com/synnaxlabs/foundation/pull/2804#issuecomment-6208160301;
  `Error::Escape`, 2026-10-16 04:58 UTC,
  https://github.com/synnaxlabs/foundation/pull/2804#issuecomment-6208423911).
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
  drop of its future, a write open of the path gives `Busy`; on `os` the lock is
  released after the unlink, so the lock holds across processes until then. The race
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
  Amended (2026-10-08T00:45:03Z, #1524): a write open (`Mode::Write` or `Mode::Create`),
  a `Files::remove`, and a `File::rename` to a path first wait for each call on the path
  that a dropped future or a dropped handle of the same `Files` and its clones left to
  run. `Busy` stays for a live handle, and for a handle in another process or another
  `Files`. `Mode::Read` does not change. This supersedes the `Busy` after a drop in the
  #392 sentence above (https://github.com/synnaxlabs/foundation/issues/392) and in the
  #1604 amendment
  (https://github.com/synnaxlabs/foundation/issues/1604#issuecomment-6046168932).
  Decided by `laptop.architect-2`, #1524, 2026-10-08T00:45:03Z
  (https://github.com/synnaxlabs/foundation/issues/1524#issuecomment-6049807436). The
  `File::remove` text: `laptop.architect-2`, 2026-10-08T01:03:54Z
  (https://github.com/synnaxlabs/foundation/issues/1524#issuecomment-6050016336). The
  texts on another `Files`, and that `sim` gives each `Files` of a node as clones of
  one: `laptop.architect-2`, 2026-10-09T14:36:39Z
  (https://github.com/synnaxlabs/foundation/pull/2156#issuecomment-6083081272). Each
  rule that a call waits has one test of the same name on `os` and on `sim`, and a rule
  that a call does not wait has its test on `sim` only, as `os` runs the calls of a
  `Files` in queue order: `laptop.architect-2`, 2026-10-09T14:41:42Z and
  2026-10-09T15:35:53Z
  (https://github.com/synnaxlabs/foundation/pull/2156#issuecomment-6083166443 and
  https://github.com/synnaxlabs/foundation/pull/2156#issuecomment-6084098381). Built in
  #2156, where each driver gives the wait: `os` by the order of its I/O queue, `sim` by
  an end time no earlier than the end of each such call and of a remove through a
  handle, where a later call with the same end time ends after it: `laptop.architect-2`,
  2026-10-09T16:10:14Z
  (https://github.com/synnaxlabs/foundation/pull/2156#issuecomment-6084674649). In
  `sim`, a call through a handle is on each path that has named its file and on the
  path of each dropped rename of the file, so `sim` can wait longer than the rule needs,
  never less: `laptop.architect-2`, 2026-10-09T17:41:15Z
  (https://github.com/synnaxlabs/foundation/pull/2156#issuecomment-6086107940). `sim`
  gives each `Files` of a node as clones of one. Lost there: the wait in `Files`, which
  cannot see the end of a dropped call without a change to `Driver`.
  Amended (2026-10-11T17:01:13Z, #1186): `Files::list` of a directory first waits for
  each call on a path directly in the directory that a dropped future or a dropped
  handle of the same `Files` and its clones left to run, so the names are as those
  calls leave them. The rule does not cover a call on the directory itself, a call on
  a path deeper in it, or a call whose future lives, also a `File::remove`, which the
  caller can await. `Mode::Read`, `sync_dir`, and `create_dir` do not change. `os`
  gives the wait by the order of its I/O queue, and `sim` by the end time of the list,
  as for a write open. The caller is `mesh::log`, whose open listed a file that a
  dropped remove then removed. Lost: a new call that waits for the dropped calls of a
  directory, which each caller that opens a directory must know to make; a wait of
  each call for each dropped call of the `Files`, which makes a write on one file wait
  for a dropped create of another, and refuses a driver that runs the calls of two
  files at one time; a second list in the caller on `NotFound`, which is a second
  guard and misses a dropped create; a wait in a read open, which a read open of a
  listed name does not need after the list waits. Decided by `laptop.architect-2`,
  #1186, 2026-10-11T17:01:13Z
  (https://github.com/synnaxlabs/foundation/issues/1186#issuecomment-6111458525).
  Amended (2026-10-11T17:05:00Z, #1229): `Files::free` first waits for each call that a
  dropped future or a dropped handle of the same `Files` and its clones left to run, on
  each path, so the result is as those calls leave the disk. It does not wait for a
  call whose future lives. `os` gives the wait by the order of its I/O queue, and `sim`
  by the end time of the `free`, as for a write open. The caller is `blob`, whose floor
  check read the free bytes before the create of a dropped put took its room. Lost: a
  wait only for the dropped calls that change the free bytes, which is a longer rule
  for a shorter wait that no caller needs; a count in `blob` of the room of each
  dropped put, which cannot go down, because no result reaches a dropped future; a
  create that takes a floor, which moves the rule of one caller into each driver.
  Decided by `laptop.architect-2`, #1229, 2026-10-11T17:05:00Z
  (https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6111493387).
  Amended (2026-10-11T22:42:41Z, #1186): each wait for the calls that a dropped future
  or a dropped handle left to run (a write open, `Files::remove`, `File::rename`,
  `Files::list`, `Files::free`) is for a future or a handle that dropped before the call
  started. A drop while the call is in flight can add the wait or not: each wait of a
  driver call for the calls that a dropped future or handle left to run is for a future
  or handle that dropped before that driver call started, and `File::rename`, and a
  create after `Full`, send a driver call after they start. A caller awaits a call, or
  drops its future, before it starts a call that must see its result. For a list, a
  `free`, a remove, and a write open (for a create, its first open), the driver call
  starts with the call, and `sim` shows the order without the wait. Its tests are on
  `sim` only, as `os` runs the calls of a `Files` in queue order. Lost: a wait for each
  future that drops while the call is in flight. A caller cannot build on it: it cannot
  know that the future will drop, and the same result comes by rule when the future
  lives. `os` gives it only for a call that started first. A driver that runs two calls
  at one time gives it for a write open only by a wait for each live call on the path.
  Lost also: no wait for each such drop, with the start time of the call as an input of
  each driver call, which changes the `Driver` surface for a result that no caller can
  use. Decided by `laptop.architect-2`, #1186, 2026-10-11T22:42:41Z
  (https://github.com/synnaxlabs/foundation/issues/1186#issuecomment-6114719100; a drop
  in flight can add the wait, 2026-10-12T15:52:54Z:
  https://github.com/synnaxlabs/foundation/issues/1186#issuecomment-6127761370; a remove
  and a write open, 2026-10-12T16:17:43Z:
  https://github.com/synnaxlabs/foundation/issues/1186#issuecomment-6128175603; each
  wait for the calls that a drop left to run, 2026-10-12T20:21:46Z:
  https://github.com/synnaxlabs/foundation/pull/2432#issuecomment-6131966723).
  Amended (2026-10-08T00:45:04Z, #1622): a `Files::open` with `Mode::Create` whose
  driver gives `Full` syncs the directory of the path one time and opens again. It
  gives `Full` only when the second open does, and a `sync_dir` error as it is. It does
  this on each `Full`, since a remove from before a restart or from another `Files`
  also holds room. `os` and `sim` do not change. Lost: the retry in each driver, and
  the rule in each caller (`blob` retried, `buffer` always synced first). Decided by
  `laptop.architect-2`, 2026-10-08T00:45:04Z
  (https://github.com/synnaxlabs/foundation/issues/1622#issuecomment-6049807597).
  `buffer` keeps its sync before the ring create until #2378 (`laptop.architect`,
  2026-10-11T08:02:51Z,
  https://github.com/synnaxlabs/foundation/pull/2375#issuecomment-6106938767).
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
  Foundation node spawns no process, so only tests see it. Lost:
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
  Amended (2026-10-09, #1732): `os::interrupt` holds SIGINT and SIGTERM. A thread of the
  module `os::signal` takes them with `sigwait` and completes a future at the first, and
  then takes them as with no hold, so a second one ends the process and a stop that
  hangs can still be ended. `main` is to call it before it starts any other thread
  (#1732), since the mask passes to each thread that starts after it. `pthread_sigmask`,
  not `sigprocmask`, which POSIX does not specify in a process with threads. Like the
  `resolve` thread of `os::net`, the signal thread has no handle. It lives until the
  process ends, to take the second signal. Tokio's `signal` lost: its handler stays for
  the life of the process and never gives back the default action, so a second signal
  cannot end the process. A handler of our own lost: it reaches its pipe only through a
  global. `os::files` makes `dir` and `dir/data` when they are not there, but not the
  parents of `dir`, and syncs the holder of each at each call, so `main` is to make no
  file call of its own and each failure of the data directory is `os::Error::Dir`
  (`laptop.architect-2`, 2026-10-09T18:30:45Z:
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545; the sync
  at each call, the thread with no handle, and the lost options: `laptop.architect-2`,
  2026-10-09T18:40:55Z:
  https://github.com/synnaxlabs/foundation/pull/2169#issuecomment-6087061440; the text
  of the thread with no handle: `laptop.architect-2`, 2026-10-09T19:05:37Z:
  https://github.com/synnaxlabs/foundation/pull/2169#issuecomment-6087447005).
  Amended (2026-10-11, #2324): `os::interrupt` changes only what a signal that reaches
  the process does, never which signals reach it. At the call, it leaves out each of
  SIGINT and SIGTERM that the mask of the calling thread blocks or whose action is
  `SIG_IGN`, also for the first signal. With both left out, no thread starts and the
  future never completes. Lost: owning both signals at each call (unblock each and set
  the default action), which overrides the choice of the parent, such as the ignored
  SIGINT of a job that a script starts with `&`. `os::files` syncs no holder of a
  directory on another device than its holder (a mount point): `mkdirat` makes a
  directory on its holder's device, so no start made that entry. Each other case keeps
  the sync at each call. Lost: no sync when `dir` was there, which opens the crash
  window again, and `EINVAL` from the sync taken as success, which hides a file system
  that cannot keep the node's entries (`laptop.architect-2`, 2026-10-10T23:40:43Z:
  https://github.com/synnaxlabs/foundation/pull/2169#issuecomment-6103406164).
  Supersedes, for a signal that the calling thread blocks or ignores, the hold of SIGINT
  and SIGTERM of
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545, and, for
  a directory on another device than its holder, the sync of the holder at each call of
  https://github.com/synnaxlabs/foundation/pull/2169#issuecomment-6087061440.
  Amended (2026-10-11, #1744): `env::paths` gives the files of the host by the paths
  that a user gave, as on a command line: a read of a file or of a directory's entries,
  up to a bound, and a write of a whole file. Each call blocks its thread. A
  `Paths` goes only to `ops::cli`, never to a shard of a node or to `ops::serve`, so a
  path that a remote program names never reaches the disk of the node. "Of a node" is
  by `laptop.architect-2` (2026-10-13T02:14:36Z, item 3 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136725489;
  approved by `laptop.architect`, 2026-10-13T03:51:37Z,
  https://github.com/synnaxlabs/foundation/pull/2533#issuecomment-6137828742), because
  `cli` runs on a shard of the command line. Supersedes "never to a shard" of item 3 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106994274. `os`
  writes a file beside the path and renames it over the path. `sim` keeps one tree in
  memory for each node, apart from its disk, where a directory is there while a file is
  under it, and `sim::Node::fail_path` fails one call with a code. Lost: `ops` takes
  `env::files` for the user's files (that seam is one directory, refuses an absolute
  path and `..`, and starts a thread for each directory); a trait in `ops` that `main`
  implements (its real half would reach the OS outside `os`). Decided by
  `laptop.architect-2` (2026-10-11 07:10 UTC, plan revision 4, decision 4,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106546913), with
  the `ops::cli` sentence by `laptop.architect` (2026-10-11 08:10 UTC,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106994274). One
  bound holds for a read: the bytes of a file, or the bytes of the entry paths of a
  directory. Past the bound, the read gives `Found::Large` with the `Kind` of what is at
  the path, so a caller tells a large file from a large directory with one read. Lost:
  two variants `LargeFile` and `LargeDirectory`, whose names repeat "Large"; a field
  `directory: bool`, a bool where a type belongs (`laptop.architect-2`,
  2026-10-17T04:33:58Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6229681928). A path
  that ends in `/`, `.`, or `..` names a directory, and the empty path names nothing.
  `env::paths::names_directory` holds the rule for the text of a path, and each driver
  asks it. Lost: a copy in each driver. The copy of `os` lost its `..` arm once, and the
  cases that both drivers share passed (rounds 4 and 5 of #2424; `laptop.architect-2`,
  2026-10-11T20:23:25Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6113352507, item 2).
  It splits the text where `std::path::is_separator` says, so on Windows it also splits
  at `\`. Each other rule of a Windows path, as a drive, a device name, or a verbatim
  prefix (`\\?\`, where `Path` splits at `\` only), is for the Windows driver of
  `os::paths`, which is not there yet. Lost: a split at `/` only, which on Windows takes
  `a\` as the name of a file (`laptop.architect-2`, 2026-10-11T20:49:35Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6113601546; the
  verbatim prefix, 2026-10-11T23:02:02Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6114877721). On `os`,
  the permission bits of a file that a write makes give the group and other users no
  access, also when it replaces a file, and a symbolic link, a device, or a pipe at the
  path takes the bytes in place, with no rename. Lost: a second bound for the count of
  entries; a rename over each thing at the path, which replaces `/dev/null` with a file
  for a root user (`laptop.architect-2`, 2026-10-11T15:18:50Z:
  https://github.com/synnaxlabs/foundation/pull/2411#issuecomment-6110509058). A write
  is whole or not at all only when a file or nothing is at the path. In place, a crash
  or a failed write can leave a part of the bytes, and a write on a symbolic link to
  nothing gives `NotFound` and makes no file. Lost: `create(true)` in the open in place,
  which makes a file at a path that the user did not name (`laptop.architect-2`,
  2026-10-11T15:51:21Z:
  https://github.com/synnaxlabs/foundation/pull/2411#issuecomment-6110806235; the docs,
  2026-10-11T16:24:16Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6111115118). In place,
  a reader can also see a part of the bytes, and two writes at one time can leave a mix
  of the two (`laptop.architect-2`, 2026-10-11T16:37:30Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6111239158). On `os`,
  the file that a write makes is a new file of the user, also when it replaces a file.
  It takes nothing from the old file: not its owner, its group, its permission bits, or
  its access control list. In place, `os` sets none of them. Lost: the permission bits,
  the owner, and the group of the old file kept on the new one. A group that the user
  cannot set, the special bits, an access control list, and the other users of a file
  whose group is not kept each need a rule of their own, and a gap in one opens the file
  to a user that the old file did not. Also lost: a write in place on each file that is
  there, which keeps each of them and gives up the whole write. The cost: an operator
  that opens a file to a group opens it again after each write, as after the first. Look
  again when a caller writes a file that the user made, as a formatter of config files.
  The hidden file has a name of 21 bytes, and a write gives `Io` where the OS refuses
  its path as too long. Lost: the open and the rename through a handle of the directory,
  which needs read access to the directory, or an open flag that Linux and macOS do not
  share (`laptop.architect-2`, 2026-10-11T17:24:53Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6111674956).
  Supersedes "keeps its mode" of
  https://github.com/synnaxlabs/foundation/pull/2411#issuecomment-6110509058 and the
  kept bits, owner, and group of
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6111239158. `os` reads
  and sets no access control list. A file that a write makes takes the entries that its
  directory gives to its new files, as each file that a tool makes there does. On macOS
  such an entry is then on the file, also when the file that it replaces had none, and
  it can open the bytes to another user (measured on macOS 27.0.1, round 6 of #2424). On
  Linux such an entry is on the file too, but the mode `0o600` of the open sets the mask
  of the list to no access, so the entry gives its user nothing until the operator
  widens the mask. A file that a tool makes there with a wider mode gets a wider mask
  (measured on Linux 7.0.0-1013-aws, ext4, #2504; `laptop.architect-2`,
  2026-10-12T20:42:03Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6132261488).
  The entry is the operator's (`docs/security.md`, "Other local user"). Lost: an empty
  list on the hidden file on macOS. `rustix` and `libc` give no call for it. A clear
  after the open leaves a time in which a user that the entry names opens the file, and
  that user reads the bytes through its handle. It also removes the entries that the
  operator gave the directory for its new files. The look at a caller that writes a file
  that the user made takes this case too. Supersedes "it gives no user more than the old
  file gave, with or without a list" of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6111675216
  (`laptop.architect-2`, 2026-10-11T20:23:25Z:
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6113352507).
  Amended (2026-10-12T16:30:01Z, #1930): in `os`, a drop stops a file call until the I/O
  queue takes the call. The queue takes it at a poll, when it has room and the task has
  Tokio budget left, so a call that was polled can have no effect while the queue has
  room. `os` keeps this: `env::files` gives a dropped call both outcomes. #1534 makes
  `sim` give both for each dropped call that has not ended, by a coin, also after a
  later call of the node ended. Lost: the send outside the Tokio budget, which closes
  one of two ways to the same outcome, so `sim` must give it in each case, and which
  ties the rule of `sim` to the queue of one driver; a test in `os` of the poll with no
  budget, which pins an outcome that the contract only permits. Supersedes rule 2 of
  https://github.com/synnaxlabs/foundation/pull/1462#issuecomment-6043752778. Decided by
  `laptop.architect-2` (2026-10-12T16:30:01Z:
  https://github.com/synnaxlabs/foundation/issues/1930#issuecomment-6128368790).
  Amended (2026-10-12T16:50:09Z, #1930): each file that `os` opens is closed on exec,
  and `os` releases the `flock` lock of a file itself, before it closes a descriptor
  that a write open locked. An unlock that the filesystem fails, as an NFS `LOCKU` or a
  FUSE daemon can, leaves the lock to the close, so the window stays on that filesystem.
  A lock lasts until an unlock, or until each copy of its descriptor closes, and a child
  of the process holds a copy of each open file from its fork to its exec, or to its
  end. So a lock that only the close released made a write open after a close give
  `Busy` in that window, on Linux and on macOS, against `File::close` of `env::files`. A
  failed create frees its blocks before its unlink for the same cause. The copy still
  keeps the room of a removed file until the copy closes. `os` cannot free that room at
  the remove: until `sync_dir` ends, a crash can undo the removal, and the file must
  then be whole. `os` keeps this gap, and the doc of `os::files()` states it: a
  Foundation node spawns no process, so only tests see it. Lost: the lock window as a
  gap on record with a rule for tests, which leaves `File::close` false in `os` where
  one unlock makes it true, and which each test that starts a child must know; a lock of
  the process (`fcntl`), which a child does not get, but which gives no `Busy` between
  two `Files` of one process, and which the close of any descriptor of the file drops; a
  second try of a `Busy` open, which hides the hold of another process; a hold of each
  removed file until the sync of its directory, to free its blocks then, which is state
  in `os` for a case that only tests see; a list of the failures of the unlock from
  outside, with a panic on each other error, which panics on an errno that a FUSE daemon
  or a seccomp filter gives and the list does not name. Trigger: the first change that
  makes a node spawn a process closes the room gap on the spawn side, with the macOS
  gaps of the amendment of #2000. Decided by `laptop.architect-2` (2026-10-12T16:50:09Z:
  https://github.com/synnaxlabs/foundation/issues/1930#issuecomment-6128674782;
  2026-10-12T18:18:49Z:
  https://github.com/synnaxlabs/foundation/issues/1930#issuecomment-6130043360).
  Amended (2026-10-12T19:51:51Z, #2481): a read goes through one open of its path. `os`
  lists a directory through the handle of that open (`rustix::fs::Dir::new`), so a later
  rename or replace at the path does not change what it reads. A directory that is
  removed after the open lists no entries. Lost: `Dir::read_from`, which makes a second
  open through the handle; a check of the device and inode after a second open by name,
  with a retry (`laptop.architect-2`, 2026-10-12T19:51:51Z:
  https://github.com/synnaxlabs/foundation/issues/2481#issuecomment-6131521237). The doc
  promises the one open, not what the open gives: on Linux 7.0 ext4, the kernel's open
  of a symbolic link that a rename swaps can give the directory that holds the link
  (#2490). `a_read_of_a_path_that_swaps_a_directory_for_a_file_gives_one_of_them`
  matches only the variant of the read, so #2490 does not make it flake. A later change
  must not make it assert which directory it read. Lost: a path walk of our own, which
  copies the kernel's rules as a second guard for a defect that is not ours; a check of
  the handle's device and inode against a `stat` of the path after the open, with a
  retry, which cannot tell a swap from the defect (`laptop.architect-2`,
  2026-10-12T20:23:51Z:
  https://github.com/synnaxlabs/foundation/pull/2486#issuecomment-6131997012).
  Supersedes "It reads what `path` names at its open" of item 2 of
  https://github.com/synnaxlabs/foundation/issues/2481#issuecomment-6131521237.
  Amended (2026-10-16T04:52:58Z, #2526): `Paths::within` gives the same files, where
  a read or a write of a path that does not resolve under one of its folders, or that
  resolves under one of its hidden folders, fails with `paths::Error::Outside` and
  reaches no file. A handle from `within` keeps the limits of the handle it came from.
  A path that does not resolve fails with the error of its resolve. `Paths::resolve`
  gives the absolute path with each `.`, `..`, and symbolic link resolved, and keeps a
  last name that is not there on its resolved directory. `Paths::home` gives the home
  directory of the user, resolved. `os` resolves with `std::fs::canonicalize`. On
  macOS it takes `..` after a file as the directory above it, as `realpath` does
  there, and an open of that path fails. A path that ends in `/` or `/.` on a file
  gives `ENOTDIR` on both OSes. `sim` gives each host symbolic links
  (`sim::Node::link_path`), a working directory (`sim::Node::set_working_dir`), and a
  home (`sim::Node::set_home`). The check and the call are two steps: a link that
  another program makes between them can lead the call out. `foundation mcp` gives its
  tools the `Paths` of `within`, and the command line the free one. Lost: a check in
  `ops` at each call site, which each new site must repeat; `cap-std`, which opens
  through each folder with no gap between the check and the open. Trigger: a tool
  that can make a link, or a server beside an untrusted writer. Decided by
  `laptop.architect-2` (2026-10-16T04:52:58Z, items 1 and 8 of
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208362869; the
  limits that a handle keeps and the error of a path that does not resolve,
  2026-10-16T05:41:58Z, item 2 of
  https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6208939762), after
  the decision of `laptop.monitor` (2026-10-16T04:51:22Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208345441).
  Amended (2026-10-16T05:41:58Z, #2526): `within` resolves each of its folders as
  `Paths::resolve` gives it, and fails with the error of that resolve, so a hidden
  folder named by a relative path or through a link still hides its files. The cost
  of the resolve error of a path: a tool can learn whether a directory outside the
  folders is there. Decided by `laptop.architect-2` (2026-10-16T05:41:58Z, items 1
  and 2 of https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6208939762).
  Amended (2026-10-16T06:56:55Z, #2526): a resolve and a read on `sim` through a
  directory that is not there, or of a link to nothing, give `NotFound`, as `os` does,
  and `sim::Node::make_dir` makes an empty directory. `Paths::home` stays a call of
  the driver, since the home is an OS fact with its own fallbacks, not only `HOME`. On
  a file system that ignores case, a hidden folder that is not there yet does not hide
  a name in other case, because a resolve keeps a last name that is not there as
  given. Deferred to PR 1 of #2526: `within` gives `Error::NotFound` for a hidden
  folder that is not there, and `foundation mcp` makes the data directory before its
  `within`. Decided by `laptop.architect-2` (2026-10-16T06:56:55Z, items 1 to 5, A,
  and B of https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6209932709).
  Amended (2026-10-16T07:06:00Z, #2526): `sim` keeps each directory as an entry. A
  directory is there once `sim::Node::make_dir` makes it, and `sim::Node::link_path`
  makes each directory above its link. A call through `Paths` on `sim` through a
  directory that is not there, or of a link to nothing, gives `NotFound`, also a write,
  as `os` and `Paths::write` give. On macOS, `os` gives `ENOTDIR` for a resolve that
  ends in `/`, `/.`, or `/..` after a file, also through a link, as Linux does, because
  it stats the path after `canonicalize`. Supersedes "a directory is there while a file
  is under it" of the `sim` tree above, and "On macOS it takes `..` after a file as the
  directory above it, as `realpath` does there, and an open of that path fails" of the
  amendment of 2026-10-16T04:52:58Z. Lost: the `sim` rule, since with it a write on
  `sim` made its directories and gave `Ok` where `os` and `within` give `NotFound`.
  Decided by `laptop.architect-2` (2026-10-16T07:06:00Z,
  https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6210062728; the `os`
  stat, 2026-10-16T08:00:56Z,
  https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6210913415).
  Amended (2026-10-16T08:08:47Z, #2526): `Paths::make_dir` makes the directory at a
  path. It makes no directory above it, and none through a symbolic link at the path,
  also when the path ends in `/`. It gives `Ok` when a directory is there, also through
  a link, `NotFound` when the directory above is not there or when the path ends in `.`
  or `..` and names no directory, and `Io` 17 (`EEXIST`) when anything else is there,
  also a file or a link to nothing at a path that ends in `/`. `os` gives `Ok` only
  after `EEXIST` with a directory there, from one private call that the `CreateDir` of
  `os::files` uses too, and makes each directory, also the data directory, with one
  mode, 0700, so the group and other users get no access. `sim` makes one directory
  only, apart from `sim::Node::make_dir`, which makes each one above. On a handle of
  `within`, a path that does not resolve, such as a link to nothing or a loop, passes
  the limits by its own place. When it still does not resolve after the make, such as a
  link to a directory whose resolved path is longer than the OS takes, it gives the
  error of its resolve, as `read` gives. `within` resolves `<hidden>/..` after the
  resolve of each hidden folder, which opens no file. It gives `NotFound` when the
  folder is not there, and `Io` 20 (`ENOTDIR`) when it is not a directory, or on `os`
  `Io` 13 (`EACCES`) when this user cannot search it. It skips a hidden folder that the
  limits of its handle refuse, since a path under that folder fails by those limits. A
  path under a hidden folder of a limit fails with the new `paths::Error::Hidden`, also
  when it is not under the folders of another limit, and `Outside` is only for a path
  not under the folders. This amendment gives the hidden check that the amendment of
  2026-10-16T06:56:55Z defers, and the call with which `foundation mcp` makes its data
  directory in PR 1 of #2526. Supersedes "or that resolves under one of its hidden
  folders, fails with `paths::Error::Outside`" of the amendment of 2026-10-16T04:52:58Z.
  Lost: a check that `within` makes the hidden folder, which writes in a call that only
  limits; a resolve of `<hidden>/.` as the check, which `os` resolves to `Ok` when the
  folder is not there; and a read of the folder, which blocks on a pipe and fails on a
  folder with no read bit. Lost also, for the error of the resolve after the make: a
  fallback by errno, which puts OS codes in `env`, and a driver mode that does not
  follow a link at the path after `EEXIST`, a new seam for the same result. Decided by
  `laptop.architect-2` (item B of
  https://github.com/synnaxlabs/foundation/pull/2831#issuecomment-6209932709; the plan
  of `laptop.integrator-2`, 2026-10-16T08:06:32Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6211011171, approved
  with conditions 1 to 3, 2026-10-16T08:08:47Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6211047580; the
  order of `Hidden` over `Outside`, 2026-10-16T14:34:56Z,
  https://github.com/synnaxlabs/foundation/pull/2869#issuecomment-6217809418; the
  `EEXIST` rule of `os`, the drop of end slashes, the resolve of `<hidden>/..`, `Io` for
  a hidden folder that is not a directory or that this user cannot search, the skip of a
  hidden folder past the limits, and the check of a link to nothing by its place,
  2026-10-16T15:45:37Z,
  https://github.com/synnaxlabs/foundation/pull/2869#issuecomment-6219155635; the check
  of each path that does not resolve by its own place, 2026-10-16T16:02:28Z,
  https://github.com/synnaxlabs/foundation/pull/2869#issuecomment-6219471401; the error
  of its resolve after the make, 2026-10-16T16:26:58Z,
  https://github.com/synnaxlabs/foundation/pull/2869#issuecomment-6219956876).
  Amended (2026-10-16T16:02:30Z, #2889): `Paths::read` gives `Io` 22 (`EINVAL`) for a
  file that is neither a regular file nor a directory, such as a FIFO, a socket, or a
  device, as `env::files::Files::open_absolute` gives on the `os` disk. Such a file has
  no stable content to read, and a tool or the check of `--files` must answer, not wait.
  `os` checks the kind with a stat before the open, so the open of a device that is
  there at the stat runs no driver, opens with no wait for a writer of a FIFO, and
  checks the kind again with an fstat after the open. A link that another writer changes
  between the stat and the open can give the error of the open, such as `Io` 102 on
  macOS or 6 on Linux for a socket, and the open of a device that the link names runs
  its driver. On Linux, the walk of a link that a rename replaces during the call can
  read only a first part of its old target, also none of it, so the call can open the
  folder of the link, or a folder on the way to that target, such as `/`, and the rest
  of `path` resolves under that folder. This was seen on ext4 on x86_64 in CI
  (https://github.com/synnaxlabs/foundation/actions/runs/38937329327/job/117096459445),
  and on ext4 in the Linux VM of Docker Desktop (linuxkit 5.15.49, aarch64) on an Apple
  M3 Max laptop in 9 of 100 runs of the swap test without its `Directory` arm, and not
  on overlayfs or macOS. That writer can already change what the node reads
  (`docs/security.md`, "Other local user"). `os::paths` and `open_absolute` of
  `os::files` share this one check. `open_absolute` refuses a directory at the stat with
  21 (`EISDIR`), so a directory that this user cannot read gets the advice for a
  directory. `Files::open` in the data directory still waits for a writer of a FIFO
  (#2678). `sim` models no such file. Supersedes "blocks on a pipe and" in the reason
  that the amendment of 2026-10-16T08:08:47Z gives against a read of the hidden folder:
  a read of a pipe now gives `Io` 22. `write` keeps its rule: a pipe or a device at
  `path` takes the bytes in place. Lost: an open with `O_NONBLOCK` that then reads the
  FIFO, which gives what a writer put there by chance, or nothing, and still runs the
  driver of a device; an `O_PATH` open on Linux with a reopen through `/proc/self/fd`,
  since macOS has none and keeps the race; and 22 for each error of the open after a
  stat that saw a regular file, which hides real errors such as 13. Decided by
  `laptop.architect-2` (2026-10-16T16:02:30Z,
  https://github.com/synnaxlabs/foundation/issues/2889#issuecomment-6219471919; the
  race, 2026-10-16T16:37:23Z,
  https://github.com/synnaxlabs/foundation/pull/2893#issuecomment-6220151438; the
  directory, 2026-10-16T16:47:51Z,
  https://github.com/synnaxlabs/foundation/pull/2893#issuecomment-6220333948; the Linux
  walk, 2026-10-16T17:27:15Z,
  https://github.com/synnaxlabs/foundation/pull/2893#issuecomment-6221017304; the
  endings for a last link and the Linux-only test arm, 2026-10-16T17:35:27Z,
  https://github.com/synnaxlabs/foundation/pull/2893#issuecomment-6221156481; the folder
  of the link and the evidence, 2026-10-16T17:48:30Z,
  https://github.com/synnaxlabs/foundation/pull/2893#issuecomment-6221386763).
  Amended (2026-10-16T17:13:18Z, 19:43:31Z, 20:05:43Z, 20:19:38Z, 20:27:04Z, 20:41:21Z,
  and 21:03:02Z, #2898): `Paths::within` compares files, not names. It keeps the
  resolved path of each folder of `folders` and `hidden`, and each call takes the
  `paths::Key` of each folder again, from the new `paths::Driver::key`: (`st_dev`,
  `st_ino`) of a stat on `os`, and on `sim` an inode that each entry gets when it is
  made, and a new one when a write replaces a file, as the rename of a write of `os`
  gives. The device of each `sim` key is 0, since a key is compared only on its node. A
  path is under a folder when the key of the path, or of a directory on its way to `/`,
  is the key of that folder. The key of the path itself comes from the path that the
  caller gave, since the resolved path of a new name can be longer than the OS takes;
  the keys of the directories come from the resolved path. `NotFound` at the path
  itself, the last name of a path that a write or `make_dir` makes, is no key, and each
  other error is the error of the call. So a name in other case on a file system that
  ignores case, and a bind mount or the `/System/Volumes/Data` prefix on macOS, reach
  the hidden folder and give `Hidden`, and the same name of a folder passes where it
  gave `Outside`. A path also gives `Hidden` when a directory on its way is the file
  that was at a hidden folder at `within`, so a hidden folder that a program moves away
  stays hidden, but not the listing of the moved folder itself. That key is never
  compared with the key of the path itself, since a write by rename frees the inode of a
  hidden file and ext4 gives it again to a new file. After a hidden folder is removed, a
  directory that the system gives its old key hides what it holds. While nothing is at a
  hidden folder, a call that makes a new name in the deepest directory on its way that
  is there gives `Hidden`, also for a name that does not make that folder. So no name in
  other case, in other Unicode form, or through the `/System/Volumes/Data` prefix makes
  it, and no folding rules are needed. The cost is that a new file beside a missing
  hidden folder is refused. So a hidden file that a write replaces, a hidden folder that
  a program makes again, and one that is made after `within` stay hidden. `make_dir` of
  a path that does not resolve checks its resolved directory, since no folder is at a
  name that does not resolve. A folder of `folders` that is not there gives `NotFound`
  at `within`. A folder of `folders` keeps the key of what was there at `within`, and
  holds a path only while the file at its resolved path has that key, so a folder that a
  program removes, renames away, replaces with a link, or makes again holds nothing
  until a new `within`. A folder that a program removes and makes again holds again
  when the host gives it the old key, as ext4 can. A write replaces a file, so a
  folder that is a file holds
  nothing after a write to it, also through this handle. A folder of `hidden` that is
  not there gives no error, also when the limits of the handle refuse it, and one that
  is there gives its key, also when the limits refuse it. That key comes from the path
  that the caller gave, at `within` and at each call, as for the path of a call, so a
  later folder under it stays hidden. A folder whose key a call cannot take holds
  nothing at that call, so a hidden folder that this user cannot search fails no call
  outside it. A hidden folder that is a file, a pipe, or a folder that this user cannot
  search passes, and the path of each such file gives `Hidden`. The cost is one stat for
  each name of the resolved path, for each hidden folder, for each directory above a
  hidden folder that is not there up to the first that is there, and for each folder on
  each call of `Paths`, for tools, plans, and secrets, not frames. Supersedes the
  resolve of `<hidden>/..`, its `Io` 20 and 13, and the skip of a hidden folder that the
  limits of its handle refuse, of the amendment of 2026-10-16T08:08:47Z. Lost: the key
  of each hidden folder once at `within`, which a write by rename or a folder made again
  makes stale; the key of each folder of `folders` at each call, which a link that
  replaces the folder widens; the match of the name of a hidden folder, which a name in
  other case or other Unicode form passes on macOS; a compare of the key at `within`
  with the key of the path itself, which a reused inode makes a false `Hidden`; a
  compare of the text after case folding, which refuses a distinct folder on a file
  system that keeps case, needs the folding rules of each file system, and misses a bind
  mount; the name on disk by a read of each directory on the way, which is slow and
  misses a bind mount; and an open beneath the folder (`openat2` with
  `RESOLVE_BENEATH`), which macOS does not have. Trigger for the last: a need to close
  the race between the check and the call. A hard link to a hidden file gives `Hidden`
  while the file keeps the key of the call and its folder can be searched. A hard link
  to a file of a hidden folder, from a folder outside it, still passes: making it needs
  write access to both, which already gives the power to read the file
  (`docs/security.md`, "Other local user"). `/dev/fd/<n>` on macOS still reaches a
  hidden file that the node holds open (#2917). Decided by `laptop.architect-2` (the
  mechanism, 2026-10-16T17:13:18Z,
  https://github.com/synnaxlabs/foundation/issues/2898#issuecomment-6220778186; the
  order before PR 1 of #2526, 2026-10-16T19:36:24Z,
  https://github.com/synnaxlabs/foundation/issues/2898#issuecomment-6223241814; the
  errors of a hidden file, pipe, or folder that this user cannot search, the key of a
  refused hidden folder that is there, and the errors of `within`, 2026-10-16T19:43:31Z,
  https://github.com/synnaxlabs/foundation/issues/2898#issuecomment-6223354914; the key
  of each hidden folder at each call and at `within`, 2026-10-16T20:05:43Z,
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6223714208; `NotFound`
  at `within` for a folder of `folders` that is not there, 2026-10-16T20:19:38Z,
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6223945647; the new
  name beside a missing hidden folder, the key of a folder of `folders` at `within`, a
  key that a call cannot take, and the compare of a hidden key only with the directories
  on the way, 2026-10-16T20:27:04Z,
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6224070884; the key of
  a hidden folder at `within` from the given path, 2026-10-16T20:41:21Z,
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6224298302; the hard
  link text and a folder that is a file, 2026-10-16T21:03:02Z,
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6224644233).
  Amended (2026-10-17T05:16:48Z, #2919): `paths::Error` has two more variants, so a
  caller can give a true cause. `Vacant`: nothing is at a hidden path of `within`, and
  a write or `make_dir` makes a new name in the deepest directory on the way to it
  that is there.
  It is the refusal of a new name beside a missing hidden folder, which gave `Hidden`.
  Supersedes "gives `Hidden`, also for a name that does not make that folder" of the
  amendment of 2026-10-16T20:27:04Z
  (https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6224070884).
  A hidden folder that is there, and one that a program moved away, still give
  `Hidden`, which wins over `Vacant`. `Outside` and `Replaced` win over `Vacant`.
  A read gives `NotFound` where a write gives `Vacant`.
  `Replaced`: no folder of `within` holds the path, and a program moved or replaced a
  folder of that call after it: the folder's path names another file, or nothing, or the
  OS refuses its key. The path resolves under that folder, by the path that the folder
  resolved to at that call or by the one that it resolves to now, or into the directory
  that the folder was at that call. It replaces `Outside`, which such a path gave. So a
  folder that is a file gives `Replaced` after a write to it, and so does a folder that
  a program makes again in other case or replaces with a link, and a path into a folder
  that a program moved away. The common cases pin each on both drivers, other than a
  folder made again in other case, which an `os` test pins on macOS.
  `sim::Node::move_path` renames a file, a link, or a directory of a host from outside
  the node, with its key, so the common cases move a folder away on both drivers. When
  the node's working directory resolves, it stays on the directory that it resolves to,
  as on a host: it moves with it when that directory is `from` or under it, and a move
  of a link on its way leaves it there. It keeps its path while that path resolves to
  that directory, and else takes the path of the directory. A call is of the path of a
  fault of `sim::Node::fail_path` when both give one path from the working directory at
  that call, with `.` and `..` taken as text and no link followed, so a relative path
  follows a later `set_working_dir`. Faults that a call is of fire in the order set, one
  per call. Decided by `laptop.architect-2` (2026-10-17T05:16:48Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6230063256; the order,
  a read, `Replaced` by the path now, and `move_path`, 2026-10-17T06:33:57Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6230731316; a path
  into the moved folder and the moved working directory, 2026-10-17T07:17:06Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6231111995; the
  working directory that a move keeps on the directory that it resolves to,
  2026-10-17T07:41:22Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6231322001; the common
  cases, 2026-10-17T08:26:44Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6231727914; the
  faults, 2026-10-17T08:58:04Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6232020329; a key that
  the OS refuses, 2026-10-17T16:01:08Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6236248693, which
  withdraws item 1 of 6231727914 and item 1 of 6232020329; the path of a fault,
  2026-10-17T16:12:13Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6236404985, which
  states what one path is, as `absolute` gave it for item 2 of 6232020329: `.` and `..`
  taken as text, and no link followed; the path of the working directory on a move,
  2026-10-17T16:16:39Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6236468300, which
  supersedes "`working` becomes its resolved path when it resolves" of 6231322001 and
  "`move_entry` goes back to the rule of 6231322001, and 7c9caa232 goes" of item 2 of
  6232020329). Lost: a second look in `ops` after a refusal, which puts a `Paths` with
  no limits in the scope that limits `foundation mcp`, states the rule of `within`
  again, and gives a false text when a file changes between the two looks. Lost too: the
  error of the key of a folder that the OS refuses, which needs the codes of each host
  in `env`; the next `within` gives it. Lost too: a working directory resolved at
  `set_working_dir`, which cannot hold where nothing is there yet, so it needs a second
  rule.
  Amended (2026-10-17T06:14:25Z, #2895): `make_dir` of a path that does not resolve
  checks its name in its resolved directory, with the key of the file at that name,
  before the driver makes it. So a loop or a link to nothing beside a hidden folder that
  is not there, or in a hidden folder that a program moves away, gives `Hidden`, not
  `Io` 17, and so does a hidden file at a path that ends in `/` or `/.`. `Io` 17 still
  comes where the name passes the limits. Supersedes "`make_dir` of a path that does not
  resolve checks its resolved directory, since no folder is at a name that does not
  resolve" of the amendment of 2026-10-16T17:13:18Z. Decided by `laptop.architect-2`
  (2026-10-17T06:14:25Z,
  https://github.com/synnaxlabs/foundation/pull/3000#issuecomment-6230570417; the end in
  `/` or `/.`, 2026-10-17T06:28:33Z,
  https://github.com/synnaxlabs/foundation/pull/3000#issuecomment-6230686685).
  Amended (2026-10-17T06:14:25Z, #3006): with `Vacant` of #2919, that loop or link to
  nothing beside a hidden folder that is not there gives `Vacant`, not `Hidden`. One in
  a hidden folder that a program moved away still gives `Hidden`. Supersedes "a loop or
  a link to nothing beside a hidden folder that is not there ... gives `Hidden`" of the
  amendment of 2026-10-17T06:14:25Z (#2895)
  (https://github.com/synnaxlabs/foundation/pull/3000#issuecomment-6230570417). Decided
  by `laptop.architect-2` (2026-10-17T06:14:25Z,
  https://github.com/synnaxlabs/foundation/pull/3000#issuecomment-6230570417; the four
  cases, 2026-10-17T07:41:22Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6231322001).
