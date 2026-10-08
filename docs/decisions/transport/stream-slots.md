- **STREAM SLOTS (#2018, 2026-10-08)** A node gives the peer each freed stream at once,
  whatever the peer sends. A slot is one stream of the count `streams_max` that
  `MAX_STREAMS` gives, not bytes of stream data. The local patch of `noq-proto` 1.3.0
  (`docs/dependencies.md`) queues `MAX_STREAMS` once the limit can rise by one stream,
  also when the stream frees because the node stops it. The release waits until more
  than 1/8 of the window is free, so with `streams_max` 16 a `Session::open` that
  waited got no stream until three streams ended. Cost: `MAX_STREAMS` carries only the
  latest limit, so a burst of freed streams costs one frame per packet, not one per
  stream, and the frame goes in the packet that acks the end in the common case. Lost:
  a `MAX_STREAMS` only to a peer that sent `STREAMS_BLOCKED`, with that frame sent at
  once (a peer on another QUIC stack, or on the release, waits up to a third of the
  idle timeout for its `STREAMS_BLOCKED` to go); a `streams_max` under 8, so that 1/8
  of it is 0 (a limit that hides the cause); a retry in `transport` (the peer still
  gives no slot). Decided by `laptop.architect-2` (2026-10-08T23:25:58Z:
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6071044405), which
  supersedes the shape of
  https://github.com/synnaxlabs/foundation/issues/2018#issuecomment-6070651968.
