- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. Amended (2026-10-09, #2063): a driver of
  `tasks` that counts tasks passes each task to one of theirs, and a driver of a test or
  a benchmark may keep each task for its caller to poll (`laptop.architect-2`,
  2026-10-09T03:42:09Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6073808019).
  Supersedes the ENV SEAMS and `Driver` texts of item 4 of
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6072358016.
  Amended (2026-10-17, #2923): outside the binary, `sdk` takes its clock, entropy,
  network, and the shard of its client from `os` (`laptop.architect-2`,
  2026-10-17T00:22:08Z,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6227271021, and
  2026-10-17T17:32:45Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237701374). So a
  composition root holds `Shards`: `node`, or `sdk` for a client.
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
  gives `Unreachable`. A UDP bind to `[::]:0` first tries up to 8 ports, each from a
  new IPv4 probe on `0.0.0.0:0` that it closes. When a probe fails or no such port
  binds, whatever the error, the kernel picks: on macOS the kernel can give it the
  port of a held `127.0.0.1` socket, which then gets each datagram to that port (#3422,
  https://github.com/synnaxlabs/foundation/issues/3422#issuecomment-6276973048). A
  `Transmit` goes out in one `sendmsg`, with GSO, or one per datagram after the kernel
  refuses GSO; there is no `sendmmsg`. A `Transmit` holds at most `TRANSMIT_BYTES_MAX`,
  so a GSO batch gets `EMSGSIZE` only when its segment is over the path MTU. Then each
  full segment is lost, and `os` sends the short last datagram alone
  (`laptop.architect-2`, 2026-10-09 22:36 UTC,
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
  `os` gives the kernel's answer (on Linux, `Unreachable` or `Io { code: 22 }`). For
  port 0, `os` gives the kernel's answer. Such a transmit changes no state of the
  socket (`laptop.architect-2`, 2026-10-08 21:15 UTC,
  https://github.com/synnaxlabs/foundation/issues/1972#issuecomment-6069193130).
  Decided by `laptop.architect-2` (2026-10-08 18:56 and 19:02 UTC, #1965,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066909518,
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6067014743).
  Supersedes "or the errno" of item 2 of
  https://github.com/synnaxlabs/foundation/issues/119#issuecomment-6066429541: a
  failure is not stored. Amended (2026-10-18, #3185): for such a source, `sim` gives
  the codes that SIM NETWORK states (`laptop.architect-2`, 2026-10-18T12:49:45Z,
  https://github.com/synnaxlabs/foundation/pull/3185#issuecomment-6250115373).
  Supersedes "`sim` gives `Io { code: 99 }`" of
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6066909518 and
  "`sim` stays at 99" of item 2 of
  https://github.com/synnaxlabs/foundation/pull/1965#issuecomment-6067014743, but for
  a unicast IPv4 source that is not on the node, to an IPv4 destination, which keeps
  code 99 (#3187; `laptop.architect-2`, 2026-10-18T13:31:33Z,
  https://github.com/synnaxlabs/foundation/pull/3185#issuecomment-6250512376). One
  exception: `os` gives `Io { code: 22 }` for each IPv6 source on an IPv4 socket,
  mapped too, for an IPv6 source that is not mapped with an IPv4 destination, and for
  an unspecified source in any form. Linux skips the `IPV6_PKTINFO` of the first,
  macOS skips that of the second, and Linux reads the third as no source; each then
  sends from an address of its choice. Decided by
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
  `import` is such a caller, and the look kept this shape (IMPORT; `laptop.architect-2`,
  2026-10-18T06:03:37Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6246366941). The
  hidden file has a name of 21 bytes, and a write gives `Io` where the OS refuses its
  path as too long. Lost: the open and the rename through a handle of the directory,
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
  Amended (2026-10-17T06:28:34Z, #3005): on a handle of `within`, a path that does not
  resolve is checked by the deepest prefix of its components that resolves, with the key
  of the file at that prefix. Components drop an end `/` and `/.`. A relative path
  starts at the working directory, as `.` (2026-10-17T07:40:31Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231315185). An error
  of that check wins, `Hidden` over `Outside`; else the call gives the error of its
  resolve. So a hidden file at a path that ends in `/` or `/.`, or with a name after it,
  gives `Hidden`, not `Io` 20, on `read`, `write`, and `make_dir`; `<hidden>/none/x`
  gives `Hidden`, not `NotFound`; and a path outside the folders that does not resolve
  gives `Outside`, not `NotFound`. The arm of `make_dir` still checks its name in its
  resolved directory, and there each error of the key of the name is no key, not the
  error of the call, since an error before the check, such as `Io` 40 for a loop, tells
  what a hidden folder holds. The walk runs only after a failed resolve. Supersedes "A
  path that does not resolve fails with the error of its resolve" of the amendment of
  2026-10-16T04:52:58Z, and "The cost of the resolve error of a path: a tool can learn
  whether a directory outside the folders is there" of the amendment of
  2026-10-16T05:41:58Z. Lost: a driver call that gives the deepest resolved prefix, one
  walk but a new seam that `os` and `sim` must both keep, for a path that only fails; a
  lexical drop of the end `/` only, which leaves `<hidden>/none/x`. Decided by
  `laptop.architect-2` (2026-10-17T06:28:34Z,
  https://github.com/synnaxlabs/foundation/issues/3005#issuecomment-6230686853). The
  rule of the key of the name in `make_dir` is the text that the architecture review of
  #3000 asked for
  (https://github.com/synnaxlabs/foundation/pull/3000#issuecomment-6230706661).
  Amended (2026-10-17T07:45:44Z, #3005): on a handle of `within`, the limits hold on
  each step of every path, not only of a path that does not resolve. Take each prefix of
  the components of the path, in order, without an end `/` or `/.`, the whole path last.
  A relative path starts at the working directory: the steps of that directory come
  first, each prefix of the resolved working directory, then each prefix of the path as
  given, never a prefix of their join, since a join can be longer than the host takes
  where neither is. So a relative path and the absolute path that it names give one
  result where the host takes both. `../work/a` with the working directory `/out` gives
  `Outside`, as `/out/../work/a` does. So when the working directory is outside the
  folders and not on the way to one, a relative path gives `Outside` when a `..` takes
  the path above the working directory or its file is outside; the fix of `ops.outside`
  of #2919 names that directory, so the user gives an absolute path. A prefix resolves
  as `resolve` does, and the walk stops at the first prefix that does not resolve. Each
  prefix that resolves is checked as the whole path is, but a prefix short of the whole
  path is checked against the folders only when a `..` after it takes the path above it,
  by count: each name after it adds one, each `..` takes one away, and it is checked
  when the count goes below zero, with the names of the working directory below it
  counted first; or when nothing is there, and then also passes when it is a directory
  on the way to a folder, by key. The hidden folders hold on each prefix that passes the
  folders of each limit. A path through another mount of a folder, such as
  `/System/Volumes/Data` on macOS or a bind mount on Linux, or through a link outside
  the folders that names one, gives what the folder's own path gives, as #2898 rules.
  The name of the first prefix that does not resolve is checked at its place, in the
  resolved prefix before it, with no key, as a new name there is. The first step that
  fails gives its error. When each passes and the path does not resolve, the call gives
  the error of its resolve. So `workshop/../work/a.hcl` gives `Outside`,
  `work/data/../a.hcl` gives `Hidden`, and a loop or a link to nothing in a hidden
  folder that a program moves away gives `Hidden` on `read` and `write` too. `make_dir`
  makes the directory only when each step passes. When the path did not resolve, the
  call checks it again after it makes it, and a path with no last name, such as one that
  ends in `..`, gets the error of the driver. The check gives an error of a step apart
  from the result of the resolve, so an error of a step, such as `Io` of a key, makes
  nothing. The cost is one resolve and one check for each name of the path, and for a
  relative path one resolve of the working directory and one check for each of its
  names, on each call of such a handle. Only `foundation mcp` of #2919 makes one. The
  walk resolves each prefix, and a resolve on `os` walks each name of its prefix, so the
  cost grows with the cube of the depth: a read of a file 1905 names deep in a folder
  takes 72 s on `os` (Linux, Xeon Platinum 8488C, release), where the free handle takes
  214 µs, and the call blocks its thread for that time. Only `foundation mcp` makes such
  a call, in a process of its own, so no node waits on it. A resolve on `os` fails past
  the longest path that the host takes, which bounds the walk, and a path stops at its
  first step that does not resolve. #3018 removes this cost. The cost that stays until
  #3018: a symbolic link in the folders that names nothing passes by its own place and
  gives `NotFound`, while one that names a file outside gives `Outside`, or `Hidden` for
  a file in a hidden folder, so a tool can learn whether the target of such a link is
  there. Supersedes "the deepest prefix" rule and the rule of the key of the name in
  `make_dir` of the amendment of 2026-10-17T06:28:34Z; the check of a name in `make_dir`
  of the amendment of 2026-10-17T06:14:25Z (#2895) is now a step of the walk for each
  call, with no key, which supersedes its "with the key of the file at that name". It
  also supersedes "a path that does not resolve, such as a link to nothing or a loop,
  passes the limits by its own place" of the amendment of 2026-10-16T08:08:47Z. A step
  meets the hidden folders only when it passes the folders of each limit, so a step that
  the folders of a limit refuse gives `Outside`, also beside a hidden folder that is not
  there: with the folder `/work` and nothing at the hidden folder `/out/h`, `/out/x` and
  `/out/y` give `Outside`, whether each is there or not. A short step that is there,
  that no `..` takes the path above, and that the folders of a limit refuse, passes, and
  the end decides: `/out/h/a` and `/out/h/s/x` give `Outside`, whether `/out/h` is there
  or a program moves it away, and a link in `/out/h` that names a folder leads there, as
  a link outside the folders does. A handle of `/plans` within the handle of `/work`
  gives `Outside` for each name in `/work`. It also supersedes "also when it is not
  under the folders of another limit, and `Outside` is only for a path not under the
  folders" of the amendment of 2026-10-16T08:08:47Z, and the shape decision "`Hidden`
  wins over the `Outside` of another limit" of
  https://github.com/synnaxlabs/foundation/pull/2869#issuecomment-6217809418, and
  "`Hidden` over `Outside`" of the amendment of 2026-10-17T06:28:34Z
  (https://github.com/synnaxlabs/foundation/issues/3005#issuecomment-6230686853). This
  supersedes "in the deepest directory on its way that is there gives `Hidden`" of the
  amendment of 2026-10-16T17:13:18Z for a directory that is not under a folder. For the
  name of the first prefix that does not resolve, it supersedes "each other error is the
  error of the call" of the amendment of 2026-10-16T17:13:18Z: that name takes no key,
  since an error of its key, such as `Io` 40 for a loop, tells what a hidden folder
  holds. Lost: the check only after a failed resolve, since
  `/etc/foo/../../<base>/work/a.hcl` resolves only when `/etc/foo` is there, so it reads
  `a.hcl` or gives `Outside`, which tells whether `/etc/foo` is there; `Hidden` over
  `Outside` across prefixes, since `x/../work/data/y` then gives `Hidden` when the
  outside `x` is there and `Outside` when it is not; a step at or above the end of the
  walk, which needs a second pass and refuses a link outside the folders that names a
  folder where a mount passes
  (https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6236577385); the
  check of each step that any `..` follows, since `/out/in/x/../a` through a link
  `/out/in` to a folder then gives `Outside` where `/out/in/a` reads; `Hidden` for each
  name in a directory that is not under a folder, beside a hidden folder that is not
  there, which also refuses a name that is there; `Hidden` for a path under a hidden
  folder that the folders of a limit refuse, which tells whether that folder is there
  and which names it holds; `Hidden` over the `Outside` of another limit, whose fix
  takes one step fewer, since a handle within another then tells which hidden folders
  the other has outside its own folders. Decided by `laptop.architect-2`
  (2026-10-17T07:45:44Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231358043; the check
  of the name at its place and the arm of `make_dir`, 2026-10-17T07:54:32Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231433128; the
  working directory as the first steps of a relative path, and the error of a step apart
  from the resolve, 2026-10-17T08:13:25Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231604694; the cost
  and its deferral to #3018, 2026-10-17T08:19:29Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231662179; the walk
  of a relative path by its own prefixes, 2026-10-17T08:53:56Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231983300; `make_dir`
  of a path with no last name, 2026-10-17T08:44:48Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6231896085; the walk
  of the working directory first, 2026-10-17T16:01:56Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6236262586; the check
  of a short step against the folders only before a `..`, 2026-10-17T16:24:25Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6236577385; the check
  of a short step where nothing is there, 2026-10-17T16:30:59Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6236681131; the count
  of the `..` that takes the path above a step, 2026-10-17T16:38:50Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6236820865; the
  folders of a limit before its hidden folders, 2026-10-17T16:59:11Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237152656; the hidden
  folders only on a step that the folders of each limit hold, 2026-10-17T17:16:50Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237413267; the
  `Outside` of another limit over `Hidden`, 2026-10-17T17:27:38Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237608685; the
  supersede of "`Hidden` over `Outside`" of 2026-10-17T06:28:34Z, 2026-10-17T17:38:58Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237821711).
  Amended (2026-10-17T20:57:47Z, #3005, with #2919): with `Vacant` and `Replaced` of
  #2919, a step that the folders of a limit refuse gives `Replaced` in place of
  `Outside` when it resolves under a folder of that limit that a program moved or
  replaced, as the whole path does: with the folder `work` made again, `work/../b.hcl`
  gives `Replaced`, where `b.hcl` gives `Outside`. Only the whole path can give
  `Vacant`, since a call makes no other name, and only after it passes each other limit.
  A step short of it, and a read, pass where a write gives `Vacant`, so beside a hidden
  folder that is not there, `work/none/x` and a path below a loop or a link to nothing
  give the error of their resolve. Supersedes the order "`Hidden`, then `Outside` or
  `Replaced`" of item 1 of
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6230731316:
  `Replaced`, as `Outside`, comes before `Hidden`. Decided by `laptop.architect-2`
  (2026-10-17T20:57:47Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6240944707;
  `Replaced` before `Hidden`, 2026-10-17T21:04:57Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6241042085).
  Amended (2026-10-17T21:25:15Z, #3005): a step short of the path that a hidden folder
  holds is checked against the folders, as one before a `..` is, and passes them only
  when they hold it or it is a directory on the way to a folder. It still gets no
  `Hidden` unless it passes the folders of each limit, as the ruling of
  2026-10-17T17:16:50Z
  (https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237413267) rules. So
  with `wide` within `work` and the hidden `work/data` and `work/sub/s`, and `narrow`
  within `work/sub` from `wide`, `narrow` gives `Outside` for `work/data/l/a.hcl` and
  `work/data/l/s/a` through a link `l` to `work/sub`, as for `work/data/none/a.hcl`,
  where `wide` gives `Hidden`. With one limit, a path through a hidden folder outside
  the folders (`/out/h/l/a` with `l` a link to `/work`) gives `Outside`, where it gave
  `Ok`. Supersedes "passes, and the end decides" and "a link in `/out/h` that names a
  folder leads there, as a link outside the folders does" of the amendment of
  2026-10-17T07:45:44Z for a step that a hidden folder holds, and "through a link
  outside the folders that names one" of that amendment for a link in a hidden folder
  (https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6237413267, item 3).
  Decided by `laptop.architect-2` (2026-10-17T21:25:15Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6241334906).
  Amended (2026-10-17T21:54:23Z, #3005): a step short of the path that the folders check
  passes as a directory on the way to a folder only while that folder is there, and else
  gives `Replaced` when its path is on the way to the path of a folder that a program
  moved or replaced. So with the folder `work/data/pub` and the hidden `work/data`,
  after a program moves `pub` to `work/old`, `work/data/pub/a` gives `Replaced`, where
  it gave `Hidden`. A step before a `..` is one too: with the folders `work/data/pub`
  and `work/b`, after the same move, `work/data/../b/f` gives `Replaced`, where it read.
  Supersedes "and then also passes when it is a directory on the way to a folder, by
  key" of the amendment of 2026-10-17T07:45:44Z and "or it is a directory on the way to
  a folder" of the amendment of 2026-10-17T21:25:15Z for a folder that is not there.
  Decided by `laptop.architect-2` (2026-10-17T21:54:23Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6241808609; the
  supersede, 2026-10-17T22:18:40Z,
  https://github.com/synnaxlabs/foundation/pull/3016#issuecomment-6242108210).
  Amended (2026-10-18T00:20:52Z, #3018, with #3077): `Paths` walks a path name by name,
  as a host does, and `Paths::resolve` is this walk. `Driver::resolve` and `Driver::key`
  go. `Driver::absolute` gives the absolute path that the host walks: on a host other
  than Windows, it keeps each `..`. The walk takes an end `/` or `/.` from the text of a
  path before `absolute`, since `std::path::absolute` drops an end `/.`. `Driver::entry`
  gives what one `lstat` gives at a path, with no link at its last name followed: a link
  with its key and its target as written (the key gives a link step a file, so the check
  asks it as a step that changes where the path ends; the key of a link meets no folder
  or hidden key, since those come from the ends of walks), a directory, or another file,
  each with its key. A link puts the names of its target before the rest, from the root
  of the `absolute` of a target with a root, else from the directory of the link. Past
  `LINKS` (40) links, the walk gives `Driver::looped`, the code of the host on the
  normal `Io` path (`Io` 40 on Linux and `sim`, 62 on macOS), so `ops` maps no new
  variant. A host can take fewer: macOS stops at 32 (`MAXSYMLINKS`), so a path through
  33 to 40 links passes the walk and the call fails with `Io` 62 from the host. The host
  reaches the file; the walk only checks. On Windows, which `os` has no driver for
  before #2956, and where #2956 tests each rule on the Windows host: `absolute` gives
  the path that Win32 makes (`GetFullPathNameW`). A path with only a root starts at the
  root of the current drive, one with only a drive (`C:a`) at the working directory of
  that drive, and each `..` is taken by text, as Win32 takes it before it follows a
  link, so `C:\..\work\a` is `C:\work\a`. `entry` gives a junction and a symbolic link
  as `Link`, with the target as the absolute path that Windows follows: a relative
  target on the directory of the link, with each `..` taken by text. A reparse point
  that is not a name surrogate (a cloud file) is the file or directory that its
  attributes give, and another name surrogate fails with `Io`. A `\\?\` path takes `..`
  as a name, so `absolute` fails with `Io` 123 for one with a `.` or `..` name. A driver
  gives the walk the names as the host takes them, and where it cannot, it fails with
  `Io`. A disagreement that a driver misses is a defect of that driver. A link to
  nothing at the last name resolves as its target does, so a handle of `within` checks
  it at its target, where it can give `Outside`. A handle of `within` checks each step
  of the walk in order, also each step of a link target. `Paths::home` walks
  `Driver::home`, which gives the path as the host gives it. The state of a folder is
  the `entry` of its resolved path, so a link that a program puts in place of a folder,
  or of a directory above it, is not that directory, also when it names it: after a
  rename of `work` to `real` and a link `work` to `real`, a read of `work/a.hcl` gives
  `Replaced`. Each call takes the state of each folder once, and walks each hidden
  folder from the path that the caller gave, a relative one started at `within` on the
  resolved working directory, less one name for each `..` at its start, so the deepest
  directory on its way comes from that call and is never a file (#3077). The cost, for
  each call, is one `entry` for each name of the walk, and on a handle of `within` also
  one `entry` for each folder and for each directory above it, and one walk of each
  hidden folder; a step that fails the folders also walks each folder, to tell
  `Replaced` from `Outside`. `absolute` refuses a path of PATH_MAX bytes or more as
  given, so the free handle and a handle of `within` give the same `Io` (36 on Linux and
  `sim`, 63 on macOS) at once, and the walk takes no name of it. Each name goes to the
  host by its resolved path, so a name whose resolved path is longer than the host takes
  fails with `Io`, also where the free handle reaches it by a relative path, and a
  hidden folder with such a path fails `within`. When the host refuses the last name of
  the path to the walk, such as for a resolved path longer than the host takes, the
  limits check that name at its place as a new name, and the call gives the result of
  the host. When the host refuses the walk of a hidden path at a call, other than with
  `NotFound` or the error of `Driver::looped`, or at a name after a step that is a file,
  each step that passes the folders is hidden at that call, since another name of the
  file there can reach it. `NotFound`, a step that is a file, and a loop stay as they
  are: the host reaches no file by that path. On `os` (Linux, Xeon Platinum 8488C,
  debug), a read 1900 names deep takes 150 ms and a read of a 4000-byte path of `..`
  takes 5 ms; three tests pin each under 5 s, since a read 1904 names deep took 634 ms
  on the hosted 4-core CI VM, and takes 89 s on box2 with a resolve of each prefix. A
  walk takes at most the names of 42 paths of PATH_MAX: the path, the working directory,
  and `LINKS` link targets. It keeps each step as its name and the index of its
  directory, so its memory grows with the names and not with the length of each resolved
  path. Links that a writer of the host puts in a folder can still make a walk slow,
  since each name goes to the host by its full resolved path: a read through 40 links of
  `s/../` from a directory 1900 names deep takes 5.2 s on the same host and build, and
  the run peaks at 56 MB, where the free handle takes 3 ms. With a full path in each
  step, it took 5.5 s and peaked at 257 MB. That writer is the attacker of #3094, and a
  walk by handles makes each name one call. The `within` doc states the gap between the
  check and the call: "The check and the call are two steps: a process that can write a
  symbolic link in a folder between them can lead the call to a path that the check
  refused (#3094)." The gap is a containment escape, its issue is #3094, and #3018 files
  no other issue for it. Supersedes `Driver::link` and `Driver::working` of
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6231386142; "A
  prefix resolves as `resolve` does, and the walk stops at the first prefix that does
  not resolve", "never a prefix of their join, since a join can be longer than the host
  takes where neither is", and the text from "The cost is one resolve and one check for
  each name of the path" to "whether the target of such a link is there" of the
  amendment of 2026-10-17T07:45:44Z; item 4 of the plan,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243618446, and
  "When the path did not resolve, the call checks it again" of the amendment of
  2026-10-17T07:45:44Z; "the key of a hidden folder from the given path is right: it is
  the same file, with no `Io` 63 for a resolved path past PATH_MAX" of
  https://github.com/synnaxlabs/foundation/pull/2915#issuecomment-6224298302, and "not
  hidden" for a refused hidden path; the end `/` part of change 2 of
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243660689; "from
  the new `paths::Driver::key`: (`st_dev`, `st_ino`) of a stat on `os`" of the amendment
  of 2026-10-16T17:13:18Z; "it stats the path after `canonicalize`" of the amendment of
  2026-10-16T07:06:00Z; and "`Paths::home` gives the home directory of the user,
  resolved. `os` resolves with `std::fs::canonicalize`" and "The check and the call are
  two steps: a link that another program makes between them can lead the call out" of
  the amendment of 2026-10-16T04:52:58Z. Lost: a fix of only the last link, which leaves
  a leak through each step of a link target (`work/to-deep` to `work/data/a/b`);
  `resolve` that follows a link to nothing, with no new item, which leaks the same;
  `Driver::walk`, two copies of the host walk that can disagree; `Driver::link` and
  `Driver::key`, two calls for each name where one `lstat` gives each fact; a new
  `Error::Loop`, which changes `ops` and the code of the free handle; a walk that sends
  the link past `LINKS` to the host, which starts a new count there, so a chain of 41
  different links resolves; cap-std, which walks one base by open handles and closes the
  gap, but needs handles in `Driver` and in `sim` (#3094); the second check of
  `make_dir`, which makes the directory and then fails with the error of the first walk;
  D2, a key that follows links for hidden paths, which adds back a `Driver` item that
  #3018 removes and a second walk by the host that can disagree with the walk, and still
  hides nothing for `Io` 13; for a relative hidden path after the working directory
  goes, the path as `Driver::absolute` gives it at `within`, whose walk takes the
  removed directory before the `..` and so ends `NotFound` and hides nothing, as the
  host names nothing there, and an `absolute` that fails as `Refused`, which hides each
  path while the working directory is gone and keeps a hidden path that moves with a
  later change of the working directory. Decided by `laptop.architect-2` (the shape,
  2026-10-17T07:49:05Z,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6231386142; the two
  conditions of `laptop.monitor` for the plan, 2026-10-17T07:49:57Z,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6231393232; `entry`,
  `absolute`, `looped`, and the surface, 2026-10-18T00:20:52Z,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243660689; the key
  of `Entry::Link` and the Windows rules, 2026-10-18T01:16:08Z,
  https://github.com/synnaxlabs/foundation/pull/3098#issuecomment-6244123264; the four
  findings of round 1, 2026-10-18T01:25:34Z,
  https://github.com/synnaxlabs/foundation/pull/3098#issuecomment-6244199067; the
  boundary of D1, 2026-10-18T02:17:50Z,
  https://github.com/synnaxlabs/foundation/pull/3098#issuecomment-6244644441; a relative
  hidden path, 2026-10-18T02:26:32Z,
  https://github.com/synnaxlabs/foundation/pull/3098#issuecomment-6244712574, from the
  plan, https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243618446,
  and https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6238302038,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6238398268, and
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6238425408), after
  the decision of `laptop.monitor` (2026-10-18T00:21:40Z, the gap and its doc,
  https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243671534).
  Amended (2026-10-18T18:28:15Z, #3094, part 1 of 2): the walk of `Paths` runs on open
  directories. `Driver::entry` goes. `Driver::root` gives the root on the way to an
  absolute path, open as a `paths::dir::Driver`. `dir::Driver::open` gives the
  `dir::Entry` at one name of that directory, never `.` or `..`, with no link followed:
  a link with its key and target as written, a directory, open, or another file, each
  with its key. `dir::Driver::up` gives the directory that the host gives above it,
  open, and the root above the root. `Driver::not_directory` gives the error of the host
  for a name in a file, as `looped` gives that of a loop. On `os`, a directory opens
  with `openat` and `O_NOFOLLOW` and `O_DIRECTORY`, with `O_PATH` on Linux and
  `O_SEARCH` on macOS, so it needs no read permission, and its key comes from `fstat` of
  that open, not of a stat before it. A name is one `openat` of a directory with no link
  followed, and the key of a directory comes from one `fstat`. When the open gives
  `ENOTDIR` or `ELOOP`, the name is also one `fstatat` with no link followed, and a link
  also one `readlinkat`, so no swap after a stat makes the open follow a link. A
  directory that a program puts at the name after the open is a file to the walk. On
  macOS, a directory that this user cannot search is open by its key alone, and each
  name in it gives `EACCES`, as on Linux. On `sim`, a directory is open by its inode,
  whose path each call finds again, so it keeps its directory after a move; `/` is inode
  0 and has no entry, and one lookup gives `/` as inode 0. The walk keeps one directory
  open: a `..` calls `up` on that directory, which must give the key of a step above
  it. The walk goes on at the nearest such step, as macOS gives `/` above
  `/System/Volumes/Data/Users`, and the `..` leaves only the step that it is taken
  at. When no step above has the key, the walk takes the `..` as a symbolic link, one of
  `LINKS`, to the path that `dir::Driver::path` gives for the directory above, with the
  names left: macOS gives the system `/usr` above `/System/Volumes/Data/usr/local`, a
  step that the walk did not pass. `dir::Driver::path` does not check the path: `os`
  gives it from `F_GETPATH` on macOS and from `/proc/self/fd` on Linux, `NotFound` for
  a path that is not absolute or at `ENOENT`, and `Io` at another error, also `Io` 13
  for a directory that it holds by its key alone, and `sim` finds the path of the
  inode. Each host can give the last path of a directory that a program moved or
  removed, so the walk of that path must end at a directory with the key that `up`
  gave, or the `..` gives `NotFound`: the host finds no directory at the path that it
  gave for the directory above, as for `dir::Driver::up`. The walk checks the key at
  the end of the same walk, so no rename between a check and the walk can send it to
  another directory. Until the walk reaches the directory of such a `..`, it does not
  know where the path ends, so each error before then is an error of the walk, not of
  its end, and a hidden path whose walk meets one is refused, also for a name that is
  not there, a step that is a file, or a loop: each step that passes the folders is
  hidden. The free handle walks the same way in `Paths::resolve` and `Paths::home`, so
  on macOS they take 40 links where the host takes 32; its `read`, `write`, and
  `make_dir` go to the host by path, with no walk, until part 2 of #3094.
  `absolute` still bounds the path as given, but a name goes to the host as one call on
  its directory, so a resolved path has no bound on its length: a new name whose
  resolved path is longer than the host takes is written as the free handle writes it,
  and a hidden file past PATH_MAX
  hides only itself, so the arm of a refused hidden path meets only a refusal such as
  `Io` 13. A directory that this user can read and not search lists its entries, and
  each name in it gives `EACCES`
  (`within_lists_a_directory_that_this_user_can_read_and_not_search`). For each key that
  it asks about, the check finds in one pass over the walk the steps that have that key
  above them, and keeps the result, so it costs the steps times the keys it asks about
  and not the steps times the depth. The keys on the way to a folder are a set, so a
  step that leaves a folder past PATH_MAX costs no scan of its depth. On `os` (Linux,
  Xeon Platinum 8488C, debug), a read through 40 links of `s/../` from a directory 1900
  names deep takes 105 ms, where the free handle takes 3 ms, and a read through 33 links
  that each leave a folder 12000 names deep and come back takes 150 ms, where the free
  handle takes 12 ms; a test pins each under 1 s. The calls (`read`, `write`,
  `make_dir`) still go to the host by path, and the gap between the check and the call
  stays until part 2 of #3094. `os` has no `paths` driver on Windows before #2956.
  Supersedes, of the amendment of 2026-10-18T00:20:52Z: "`Driver::entry` gives what one
  `lstat` gives at a path", "The state of a folder is the `entry` of its resolved path"
  (it is now the walk of that path with no link followed), "The cost, for each call, is
  one `entry` for each name of the walk", "Each name goes to the host by its resolved
  path, so a name whose resolved path is longer than the host takes fails with `Io`,
  also where the free handle reaches it by a relative path, and a hidden folder with
  such a path fails `within`", "such as for a resolved path longer than the host takes"
  of the sentence "When the host refuses the last name of the path to the walk" (a
  resolved path is no longer refused for its length,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6253541091; a name
  longer than the host takes still meets that rule), "a read
  1900 names deep takes 150 ms" (it takes 4.4 ms on the same host and build), the costs
  from "Links that a writer of the host puts in a folder can still make a walk slow" to
  "a walk by handles makes each name one call", and the lost design cap-std "(#3094)".
  Also supersedes, with the ruling of `laptop.monitor` on #3018
  (https://github.com/synnaxlabs/foundation/issues/3018#issuecomment-6243671534) as the
  reason, three lost designs: "the open and the rename through a handle of the
  directory, which needs read access to the directory, or an open flag that Linux and
  macOS do not share" of
  https://github.com/synnaxlabs/foundation/pull/2424#issuecomment-6111674956, since
  `O_PATH` and `O_SEARCH` need no read access; `cap-std`, whose trigger "a server beside
  an untrusted writer" of
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208362869 this is;
  and "a path walk of our own" for the read of #2490 of
  https://github.com/synnaxlabs/foundation/pull/2486#issuecomment-6131997012, since the
  walk now exists for containment.
  `a_read_of_a_path_that_swaps_a_directory_for_a_file_gives_one_of_them` stays. Lost: a
  stack of open directories, as cap-std keeps, where `..` takes the one above with no
  compare, which holds one descriptor for each name deep, past the limits of macOS (256)
  and Linux (1024) for the read 1900 names deep; `openat2` with `RESOLVE_BENEATH` from
  each folder, which is Linux only, refuses each `..` above a folder and each link into
  another folder that `within` passes, and cannot see where a hidden folder starts; and
  the call by path with a check after it, where the open has already run. Lost for a
  `..` that gives no step above: the parent of the names walked, which goes to
  `/System/Volumes/Data/usr` where the host gives `/usr`, so `within` passes a path
  whose file the host opens outside the folder; a table of the firmlinks of macOS, which
  changes with each release; `back` on each step between the two, which checks the data
  volume against the folders above `/` and fails; a walk again from the first name
  when the host gives no path, which `os` gave after a stat of the path, since a stat
  by path races the walk and each start again needs its own bound; and the keys of the
  walk of the host path before that walk, which walks the path twice, so a rename
  between the two sends the second walk to another directory. Decided by
  `laptop.architect-2` (the plan and its four items, 2026-10-18T18:28:15Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6253532345; the `Io`
  36 tests and the docs of the old limit, 2026-10-18T18:29:09Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6253541091; from the
  plan, https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6253482479;
  each start again with all `LINKS` links and a bound of 40 starts,
  2026-10-18T20:12:25Z,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6254599373; a `..`
  that goes on at the nearest step above with the key that the host gives,
  2026-10-18T20:19:31Z,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6254672731; the path
  of the host as a link at a `..` that gives no step above, 2026-10-18T20:32:40Z,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6254807867; the walk
  of that path with a check of its key, and `NotFound` at a key that does not match,
  2026-10-19T00:36:42Z, which supersedes each start again with all `LINKS` links and a
  bound of 40 starts,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6254599373, the
  start again at a path of `None` and the `None` of `os` at a path that ends in
  ` (deleted)`,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6254807867, and the
  stat of the path in `os` that must give the key of the directory,
  https://github.com/synnaxlabs/foundation/pull/3246#issuecomment-6255000484,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6257175034; each
  error before the key check of such a `..` as an error of the walk, and `Io` 13 of
  `os` for a directory that it holds by its key alone, 2026-10-19T00:55:52Z,
  https://github.com/synnaxlabs/foundation/pull/3316#issuecomment-6257372479; the
  refusal of a hidden path whose walk meets such an error, also for a name that is not
  there, a step that is a file, or a loop, 2026-10-19T01:07:04Z, which supersedes the
  exceptions of item 2 of
  https://github.com/synnaxlabs/foundation/pull/3316#issuecomment-6257372479, and, for
  such a walk, "`NotFound`, a step that is a file, and a loop stay as they are" of the
  amendment of 2026-10-18T00:20:52Z,
  https://github.com/synnaxlabs/foundation/pull/3316#issuecomment-6257484239),
  after the decision of `laptop.monitor` (kinds 2 and 9, with the condition that each PR
  states what `os` does on Windows before #2956 and the Windows job passes,
  2026-10-18T18:24:03Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6253489977).
  Amended (2026-10-19T00:36:42Z, #3094, part 2 of 2): `read`, `write`, and `make_dir`
  run on the directories that the walk opened, with no path, so a link that a program
  puts in a folder after the walk reaches no file. `dir::Driver` gets `list`, the open
  directory to read, as a `dir::Listing`, which gives its key and its names up to a
  bound; `file`, the file at one name, open to write in place with no link followed;
  `replace`, a new file renamed over one name; and `make_dir`, a directory at one name,
  `Ok` when a directory is there with no link followed. `file::Driver` gives the key of
  the open file and reads or writes it. `Driver::error` gives the error of the host for
  each `Refusal`, a case that `Paths` refuses with no call (`NotDirectory`, `Directory`,
  `Taken`, `Looped`, `Irregular`), and replaces `Driver::not_directory` and `looped`. A
  call that opens a file compares its key with the end of the walk: at another key, a
  program put another file at the name after the walk, and the last step is checked
  again for the call, so a file that a hidden path still reaches, renamed over the name,
  gives `Hidden`. A file is hidden by its path: a hidden file renamed with no name left
  at a hidden path is not hidden, as before the walk. The call stays in the directory
  that the walk opened, so a directory that a program moves after the walk takes the
  call, also when the move puts it on the way to a vacant hidden path. A write by
  replace opens no file at the name. `make_dir` makes no call at a path whose last name
  is `.` or `..`, at a directory, or at a link, which gives `EEXIST` for a link to
  nothing or to a file and `Ok` for a link to a directory, by the end of the walk, so a
  link that goes after the walk cannot make the call make a directory beside it. A read
  of a device, a FIFO, or a socket gives `Io` 22 with no open, and a write takes the
  bytes in place. A device that a program puts at the name after the walk runs its
  driver at the open of a read, and a socket there gives the error of the open (`Io` 6
  on Linux, 102 on macOS). A path through 33 to 40 links reaches its file also on macOS,
  since the call follows no link. Each step of the walk opens to search only on both
  OSes. A read whose end is a directory that the walk reached by a name opens that name
  in its holder to read and checks it with the key of the open, as a read of a file
  does; an end with no name (a root, or a `..`) lists through an open of `.` to read in
  that directory. A directory that this user can search and not read gives `Io` 13.
  Lost: an open to read first, which macOS makes about 12 times slower than an open to
  search (https://github.com/synnaxlabs/foundation/pull/3331#issuecomment-6258700792),
  and an order of opens for each OS, where the tests of one OS do not cover the other.
  Decided by `laptop.architect-2` (2026-10-19T03:16:13Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6258874086). The
  hidden file of a replace is a name of 16 hex digits in the directory, so a write has
  no bound on the length of its resolved path. On `sim`, a fault of `Node::fail_path`
  fires at a call when the path of its directory and its name give the path of the
  fault, by text, and at a listing when the path of its directory does. The path of a
  directory is its path from `/` with each link resolved, so a fault at a path through a
  link fires at no call. It fires at no call that `Paths` does not make, such as one
  that its walk stops before, a `make_dir` where a directory or a link is, or a write
  where a directory is. It also fires at an open of `env::files` whose path gives the
  path of the fault by text, before its links are followed. `Node::race_path` runs a
  change between the walk and the call. Supersedes, of the amendment of
  2026-10-18T18:28:15Z: `Driver::not_directory`, "its `read`, `write`, and `make_dir` go
  to the host by path, with no walk, until part 2 of #3094", and "The calls (`read`,
  `write`, `make_dir`) still go to the host by path, and the gap between the check and
  the call stays until part 2 of #3094"; of the amendment of 2026-10-18T00:20:52Z:
  `Driver::looped`, and "The `within` doc states the gap between the check and the call"
  to "#3018 files no other issue for it", and "A host can take fewer: macOS stops at 32
  (`MAXSYMLINKS`), so a path through 33 to 40 links passes the walk and the call fails
  with `Io` 62 from the host. The host reaches the file; the walk only checks."; of the
  amendment of 2026-10-16T16:02:30Z: "`os` checks the kind with a stat before the open"
  to "the open of a device that the link names runs its driver.", and "`os::paths` and
  `open_absolute` of `os::files` share this one check."; of 2026-10-11T17:24:53Z: "The
  hidden file has a name of 21 bytes, and a write gives `Io` where the OS refuses its
  path as too long." Lost: a call by path with a check of the key after the open, which
  has run the open already; `make_dir` at a link through the call, which can make a
  directory beside a link that goes after the walk; one `Driver::error` method for each
  refusal, five methods for one job; a `readable` flag on the open directory, where one
  method lists the handle or opens `.` by how the handle was opened; a private enum of
  handles in `os`, which keeps the two ways behind one public doc; and a sim fault that
  resolves the links of its path at each call, more code for a case that a test sets on
  the resolved path. Decided by `laptop.architect-2` (the plan, its four open points,
  and items 1 and 2, 2026-10-19T00:36:42Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6257175034; a file
  hidden by its path, and a directory moved after the walk, 2026-10-19T03:27:45Z,
  https://github.com/synnaxlabs/foundation/pull/3331#issuecomment-6259002422; `list`,
  `dir::Listing`, the doc of `Node::fail_path`, and the text of this amendment,
  2026-10-19T03:59:33Z,
  https://github.com/synnaxlabs/foundation/pull/3331#issuecomment-6259339873).
  Amended (2026-10-19T01:30:15Z, #3094, part 2 of 2), of "a read goes through one open
  of its path" of #2481: the walk's open of a name is `dir::Driver::entry`. A read opens
  the end of its walk with `dir::Driver::open`, which gives `dir::Opened`: a directory
  or another file, by the `fstat` of the open, with no wait for a writer of a FIFO. A
  write opens it with `dir::Driver::file`, which gives `Io` for a directory. `Paths`
  checks the last step again with the key of the open, then lists the directory or reads
  the file, so a directory renamed over the file that the walk found gives its entries.
  A directory that is removed after the walk is not found when the walk reached it by a
  name, and lists no entries on `os` and on `sim` (`Node::remove_dir`) when the walk
  reached it with no name. The walk follows a symbolic link by its text, so a magic link
  of Linux (`/proc/<pid>/fd/N`, `/dev/fd/N`) to a removed file, a removed directory, or
  a pipe gives `NotFound`. On macOS `/dev/fd/N` is a directory, and a read of one that
  is removed lists no entries. No test of `os` pins `Dir::new` against `Dir::read_from`:
  Linux opens `.` in a removed directory and lists no entries, so a loop that makes and
  removes a directory while a read lists it passes on `read_from` (box2, Linux 7.0, 3
  runs each at 50 000 cycles in 0.66 s to 1.24 s and at 100 000 in 1.82 s to 2.05 s).
  Supersedes, of the amendment of 2026-10-12T19:51:51Z: "`Dir::read_from`, which makes
  a second open through the handle", since an end with no name lists through an open of
  `.` in its directory. Lost: `file(name, access) ->
  Opened`, since a write never gets a directory; a special case for `/proc` and
  `/dev/fd` in the walk. Decided by `laptop.architect-2` (2026-10-19T01:30:15Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6257732766; the name
  `entry`, 2026-10-19T02:02:53Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6258071883;
  `Node::remove_dir` and the os loop test left out, 2026-10-19T02:15:10Z,
  https://github.com/synnaxlabs/foundation/issues/3094#issuecomment-6258219510).
