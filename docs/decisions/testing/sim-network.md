- **SIM NETWORK (2026-10-04)** `sim` replaces only the network, not the transport.
  The production carriers (QUIC through `noq-proto`, TLS over TCP, relays) run
  unchanged under simulation, which is why r5 rejected iroh. The network seam lives
  in `env` (`env::net`): `os` implements real sockets, and `sim` implements the
  simulated network with loss, delay, reorder, duplication, and partitions. `sim` does
  not depend on `transport`. `transport` owns the carriers and the session model, and
  its `Transport` trait is private. `Clock::epoch` gives the `Instant` at
  `Monotonic(0)` for libraries that take a std `Instant`. Decided by the design
  session under the architecture delegation. `Node::fail_udp` makes a UDP socket fail
  as when the OS breaks it, until the socket drops: each receive gives `EIO`, the
  datagrams that arrive at it are lost, and a send still works. Approved by the
  coordinator on #907. Built by `simulation` in #926. Amended (2026-10-06, #943):
  `link::Config::rate` limits a link to that many bytes per second, counted as IP
  packets with their IP and UDP or TCP headers. Each direction of a link sends one
  packet at a time: a packet starts when it is sent or when the packet before it has
  left, whichever is later, and leaves after its bytes at the rate. Packets sent back
  to back at one rate leave at the rate of their total bytes, so the rounding of each
  to a nanosecond does not add up. A packet sent after the rate is removed still waits
  for the packets before it. Then it takes the delay and the jitter. A power cut drops
  the packets of the node that wait to leave. With no rate, a link adds no events and
  no draws, so the digest of a run does not change. A UDP datagram takes its length
  plus 768 bytes of its socket's send buffer until it leaves its link or a power cut
  drops it. As on Linux, a send goes whole while the send buffer is empty or takes
  less than `send_buffer_bytes`, and is pending from then. When a datagram leaves and
  the send buffer is no longer full, each send that waits wakes in the same step.
  Approved by the coordinator. Amended (2026-10-07, #995): `Net::resolve` gives the
  addresses of a host name. An IP literal gives its one address with no lookup, and
  no lookup is cached. `Sim::name` sets the answer to each lookup of a name in the
  run, on any node: its addresses in order, none (`NotFound`), or a failure (`Io`
  with `EAGAIN`), after a delay on the clock of the node. A lookup reads the answer
  at its first poll and sends no packet, so a partition does not stop it. Decided by
  the architect, #995
  (https://github.com/synnaxlabs/foundation/issues/995#issuecomment-6030922608).
  From the review of #1018: a name matches in any ASCII case and with or without one
  final dot, as in DNS. An IP literal as a name panics, because no lookup reads it.
  A lookup that would end past the end of the clock never answers. The match in any
  case and with a final dot was confirmed by the architect on #1018, in place of its
  earlier exact match
  (https://github.com/synnaxlabs/foundation/pull/1018#issuecomment-6031438649). Amended
  (2026-10-07, #1255): a receive of a failed UDP socket first gives the datagrams queued
  before the fault, then `EIO`. A broken socket still holds its receive queue, so the
  queue stays readable. A pulled serial adapter takes its buffer with it, so
  `Node::fail_serial` loses its unread bytes. Decided by `laptop.architect-2`, #1255
  (https://github.com/synnaxlabs/foundation/issues/1255#issuecomment-6033324472).
  Amended (2026-10-07, #1473): `Node::fail_listener` makes a TCP listener fail until
  it drops: each accept gives the streams already in its backlog, then `EIO`. A
  connect after the fault is refused, and the streams it accepted still work. Decided
  by `laptop.architect-2` at 2026-10-07T17:07:34Z
  (https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042821949).
  Amended (2026-10-07, #1532): a connect is refused when its SYN arrives after the
  fault. A connect whose SYN the listener took, but not its ACK, before the fault
  ends `Ok`, and its stream is reset when the RST of the fault arrives, so a write
  before it is taken. An accept error of `env` leaves the listener usable, unless the
  listener is broken: then each later accept fails too. Decided by
  `laptop.architect-2` at 2026-10-07T18:01:16Z
  (https://github.com/synnaxlabs/foundation/issues/1532#issuecomment-6043781710),
  with the connect sentences at 2026-10-07T18:05:20Z
  (https://github.com/synnaxlabs/foundation/issues/1532#issuecomment-6043854311).
  Supersedes the connect sentence of
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042821949.
