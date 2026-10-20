- **INITIAL RTT (#1723, 2026-10-08)** The initial RTT of a QUIC connection is the lesser
  of 333 ms and a tenth of the idle timeout, which QUIC counts in whole milliseconds,
  rounded up, so an idle of 4.5 ms counts as 5 ms. From an idle timeout of 5 ms, with no
  round-trip sample, the first PTO comes at 3 initial RTTs and the second by 9, as PROBE
  GAP only makes the gap shorter. Both come within 9/10 of the idle timeout, so a lost
  Finished goes again before the server's idle timer forgets the connection. When the
  peer's transport parameters come before the first round-trip sample, the local patch
  of noq-proto lowers the initial RTT to a tenth of the negotiated idle timeout, the
  lesser of the two sides' timeouts. The client gets them before it sends its Finished,
  and the server in the client's first Initial, so the bound holds also when the two
  sides have different idle timeouts. Outside the bound: a dial whose client gets no
  transport parameters of the server, as the server's first answer is lost. Until they
  come, the client probes at the initial RTT and the PROBE GAP cap of its own idle
  timeout, so a node whose idle timeout is shorter than that gap forgets the dial when a
  probe in the gap is lost too (42 of the 4096 masks of the first 6 datagrams of each
  side, for a program of 30 s and a node of 1 s, on the virtual time pair of the `quic`
  tests). Each of the 42 ends with `TimedOut` at the program's idle timeout, and the
  node never connects. Lost: a node that keeps a dial for the client's idle timeout
  until its handshake ends (it keeps the state of an address that it has not validated
  for longer than its own setting); a lower initial RTT on each client (as below).
  Decided by `laptop.architect-2` (2026-10-20T05:39:16Z:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6284239465). Lost: a
  cap of PROBE GAP below the PTO base with no sample (it changes a ruled decision, and
  leaves a dialer of 1 s to a node of 30 s); this record narrowed to equal idle timeouts
  (a program dials with 30 s, so it is outside the bound with each node of an idle under
  3.33 s). Decided by `laptop.architect-2` (2026-10-20T05:06:07Z:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6283832539). An idle
  timeout of 3.33 s or more keeps 333 ms, so the node's default of 30 s does not change.
  `Config` keeps any positive idle timeout, as a short one is a fast check for a dead
  peer. The reset of the PTO backoff when keys are discarded is a local patch of
  noq-proto (#1726). Lost: a floor on `Config::idle` (it refuses a real use); a fixed
  lower initial RTT (more false handshake retransmits on a long path with a long idle
  timeout). Decided by `laptop.architect-2` (2026-10-08T01:52:12Z:
  https://github.com/synnaxlabs/foundation/issues/1723#issuecomment-6050539702, item 1).
  Before the first round-trip sample, 3 PTO is at most 9/10 of `idle` from 5 ms, so a
  dial waits `idle` for the first answer. Lost: a floor in the formula (one under 333 ms
  gives the dial a hidden bound again, and one high enough to dial a path slower than
  `idle` is a value tuned to a test). Decided by `laptop.architect-2`
  (2026-10-19T12:41:49Z:
  https://github.com/synnaxlabs/foundation/issues/1723#issuecomment-6267457867, item 1;
  text: https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6282367692). At
  4 ms or less, 3 PTO is 3 ms more than 3/10 of `idle`, as each PTO is at least 1 ms
  more than the RTT, so a dial waits that long. Lost: a lower initial RTT at 4 ms or
  less (it fixes only 4 ms, as 3 PTO is over 3 ms at any RTT); a patch of noq-proto with
  no 1 ms floor (it changes the loss recovery of RFC 9002 on each connection, for an
  idle of 4 ms or less). Decided by `laptop.architect-2` (2026-10-20T04:09:21Z:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6283161395).
