- **STREAM CREDIT (#2018, 2026-10-08)** A node gives a peer that waits for a stream the
  credit of the first stream that ends. The local patch of `noq-proto` 1.3.0
  (`docs/dependencies.md`) keeps the highest limit at which the peer sent
  `STREAMS_BLOCKED`. While that limit is at least the last `MAX_STREAMS` that the node
  sent, each freed stream goes in a `MAX_STREAMS` frame at once, also when the
  `STREAMS_BLOCKED` comes after the stream freed. Else the release's rule holds: a
  `MAX_STREAMS` frame goes once more than 1/8 of the window is free. A queued
  `STREAMS_BLOCKED` counts as a frame to send, so it goes in the next packet. The
  release only logs `STREAMS_BLOCKED` and sends it only with another packet, so with
  `streams_max` 16 a `Session::open` that waits got no stream until three streams ended.
  Cost: one `MAX_STREAMS` frame for each stream that ends while the peer waits. Lost: a
  `MAX_STREAMS` frame at each stream end (a frame also when no peer waits); a
  `streams_max` under 8, so that 1/8 of it is 0 (a limit that hides the cause); a retry
  in `transport` (the peer still gives no credit). Plan:
  https://github.com/synnaxlabs/foundation/issues/2018#issuecomment-6070581970.
  Approval by `laptop.architect-2`: pending.
