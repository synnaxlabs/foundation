- **INITIAL RTT (#1723, 2026-10-08)** The initial RTT of a QUIC connection is the lesser
  of 333 ms and a tenth of the idle timeout. From an idle timeout of 5 ms, with no
  round-trip sample, the first PTO comes at 3 initial RTTs and the second by 9, as PROBE
  GAP only makes the gap shorter. Both come within 9/10 of the idle timeout, so a lost
  Finished goes again before the server's idle timer forgets the connection. An idle
  timeout of 3.33 s or more keeps 333 ms, so the node's default of 30 s does not change.
  `Config` keeps any positive idle timeout, as a short one is a fast check for a dead
  peer. The reset of the PTO backoff when keys are discarded is a local patch of
  noq-proto (#1726). Lost: a floor on `Config::idle` (it refuses a real use); a
  fixed lower initial RTT (more false handshake retransmits on a long path with a long
  idle timeout). Decided by `laptop.architect-2` (2026-10-08T01:52:12Z:
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
