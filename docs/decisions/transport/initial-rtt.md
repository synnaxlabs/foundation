- **INITIAL RTT (#1723, 2026-10-08)** The initial RTT of a QUIC connection is the
  lesser of 333 ms and a tenth of the idle timeout. With no round-trip sample, the
  first two PTOs, at 3 and 9 initial RTTs, come within 9/10 of the idle timeout, so a
  lost Finished goes again before the server's idle timer forgets the connection. An
  idle timeout of 3.33 s or more keeps 333 ms, so the 60 s of `mesh` does not change.
  `Config` keeps any positive idle timeout, as a short one is a fast check for a dead
  peer. The reset of the PTO backoff when keys are discarded is a local patch of
  noq-proto (#1726). Lost: a floor on `Config::idle` (it refuses a real use); a fixed
  lower initial RTT (more false handshake retransmits on a long path with a long idle
  timeout). Decided by `laptop.architect-2` (2026-10-08T01:52:12Z:
  https://github.com/synnaxlabs/foundation/issues/1723#issuecomment-6050539702,
  item 1).
