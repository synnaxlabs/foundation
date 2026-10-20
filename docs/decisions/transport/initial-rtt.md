- **INITIAL RTT (#1723, 2026-10-08)** The initial RTT of a QUIC connection is the lesser
  of 333 ms and a tenth of the idle timeout, which QUIC counts in whole milliseconds,
  rounded up, so an idle of 4.5 ms counts as 5 ms. From an idle timeout of 5 ms, with no
  round-trip sample, the first PTO comes at 3 initial RTTs and the second by 9, as PROBE
  GAP only makes the gap shorter. Both come within 9/10 of the idle timeout, so a lost
  Finished goes again before the server's idle timer forgets the connection. When the
  peer's transport parameters come before the first round-trip sample, the local patch
  of noq-proto lowers the initial RTT to a tenth of the negotiated idle timeout, the
  lesser of the two sides' timeouts. The client gets them before it sends its Finished,
  and the server once it has the whole ClientHello, which can take two Initials, before
  it sends its first answer. So the bound holds also when the two sides have different
  idle timeouts, other than for a client of the longer one that loses a datagram before
  it has the server's parameters. Lost: a cap of PROBE GAP below the PTO base with no
  sample (it changes a ruled decision, and leaves a dialer of 1 s to a node of 30 s);
  this record narrowed to equal idle timeouts (a program dials with 30 s, so it is
  outside the bound with each node of an idle under 3.33 s). Decided by
  `laptop.architect-2` (2026-10-20T05:06:07Z:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6283832539). Outside
  the bound: a dial whose client has the longer idle timeout and loses a datagram of
  either side before it has the server's transport parameters. Until they come, the
  client's probes back off under the PROBE GAP cap of its own idle timeout, from its own
  initial RTT when it has no round-trip sample. So the server's idle timer can end
  before the client's next datagram comes, also when the client then gets the parameters
  and its Finished is lost. On the virtual time pair of the `quic` tests, for a client
  of 30 s and a node of 1 s, these fail where two sides of 1 s keep the dial: 42 of the
  4096 masks of the first 6 datagrams of each side; 324 of the 4096 such masks at a
  delay of 50 ms each way, 160 of them with no datagram of the server lost; and 26 of
  the 2300 masks of the first 7 datagrams of each side at a delay of 30 ms each way that
  lose the Finished. Each ends with `TimedOut`. Of the 42, none connects at a later
  probe, 24 end at 31.04 s, and 18 at 33.05 s. Only a node with a shorter idle timeout
  than its client gets it, so two sides at the default of 30 s do not. A user who sets a
  short idle timeout on a node takes this cost for each client with a longer one, and a
  program dials with 30 s. Lost: a node that keeps a dial for the client's idle timeout
  until its handshake ends (it keeps the state of an address that it has not validated
  for longer than its own setting); a lower initial RTT on each client (as below).
  Decided by `laptop.architect-2` (2026-10-20T05:39:16Z:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6284239465; text:
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6284825623,
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6284849526, and
  https://github.com/synnaxlabs/foundation/pull/3498#issuecomment-6284899511). A
  negotiated idle timeout of 3.33 s or more keeps 333 ms, so two sides at the node's
  default of 30 s do not change. `Config` keeps any positive idle timeout, as a short
  one is a fast check for a dead peer. The reset of the PTO backoff when keys are
  discarded is a local patch of noq-proto (#1726). Lost: a floor on `Config::idle` (it
  refuses a real use); a fixed lower initial RTT (more false handshake retransmits on a
  long path with a long idle timeout). Decided by `laptop.architect-2`
  (2026-10-08T01:52:12Z:
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
