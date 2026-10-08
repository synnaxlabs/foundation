- **PROBE GAP (#1415, 2026-10-08)** With no answer from the peer, the gap between two
  probes of a QUIC connection is at most the cap: 1.5 RTT on a link with an RTT over
  1.33 s; else, when the idle timeout is 25 s or less, the lesser of 1 s and a third of
  the idle timeout; else 2 s. A probe of data in flight never comes before the PTO base.
  The client's probe in a dial with no data in flight, before the server validates its
  address, keeps the release's rule, `min(pto_base * 2^pto_count, cap)` from now: that
  probe is in flight, so it sets the next gap by the rule above. A probe must reach the
  peer before its idle timeout fires, so a session lives through a cut shorter than the
  idle timeout less the cap. Such a session sends again within about one cap and a few
  round trips after the heal, whatever the length of the cut. The bound covers each
  packet number space, so also the handshake of a dial that starts in a cut. The wait
  that it bounds comes only when the data in flight fills the congestion window, so that
  only a probe can send. The local patch of `noq-proto` 1.3.0 (`docs/dependencies.md`)
  makes the PTO duration `min(pto_base * 2^pto_count, max(cap, pto_base))`, and starts
  the gap at the later of the last ack-eliciting send and the last PTO fire
  (`time_of_last_pto`). `time_of_last_pto` is cleared on an ACK that sets `pto_count` to
  0. `handle_network_change` sets `pto_count` to 0 and keeps `time_of_last_pto`, so its
  first probe is at most one PTO base after the last fire. Foundation does not call it.
  The release caps each step to the step before plus the cap, so the gap grew about 2 s
  for each probe. A cap with no start at the fire keeps an expired deadline in the past,
  and `handle_timeout` never returns. Cost: during a cut, a session with data in flight
  sends one probe each 2 s. Rejected: a probe when a datagram of the peer comes in (no
  bound when the peer sends nothing); a shorter idle timeout (the gap still grows); no
  change (the wait grows with the cut for each protocol on `transport`); a watchdog that
  pings each session with data in flight (the full window holds back the ping too); and
  `mesh` drops a silent session (#1410). Decided by architect-2
  (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6039795556,
  2026-10-07T14:11:52Z). The handshake: amended by architect-2
  (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6044520215,
  2026-10-07T18:45:16Z). The start of the gap, the full window, and no upstream report:
  plan (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6058289883),
  approved by architect-2
  (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6058428352,
  2026-10-08T11:02:00Z). The cap of a third of the idle timeout: architect-2
  (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6060065670,
  2026-10-08T12:39:55Z). The `time_of_last_pto` of a network change: architect-2
  (https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6060090043,
  2026-10-08T12:41:22Z). The probe with no data in flight: architect-2
  (https://github.com/synnaxlabs/foundation/pull/1963#issuecomment-6066780738,
  2026-10-08T18:48:48Z). Supersedes the sentence "It is never less than the PTO base."
  of https://github.com/synnaxlabs/foundation/issues/1415#issuecomment-6060065670.
