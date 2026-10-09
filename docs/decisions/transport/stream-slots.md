- **STREAM SLOTS (#2018, 2026-10-08)** A node gives the peer each freed stream at once,
  whatever the peer sends. A slot is one stream of the count `streams_max` that
  `MAX_STREAMS` gives, not bytes of stream data. The local patch of `noq-proto` 1.3.0
  (`docs/dependencies.md`) queues `MAX_STREAMS` each time the limit can rise, also when
  the stream frees because the node stops it. With the release alone and `streams_max`
  16, a `Session::open` that waited got no stream until three streams ended. Cost: the
  frame goes alone in a new packet, as the ACK that frees a two-way stream needs no ACK,
  so one request and reply cost 2 datagrams each way, where the release sends 1.
  `MAX_STREAMS` carries only the latest limit, so a burst of freed streams costs one
  frame per packet. Lost: the frame at once only while the peer has opened each stream
  of the last sent limit, or when more than 1/8 of the window is free (shape (c), the
  cost of the release below the limit; the node counts a stream only once its first
  frame arrives, so a peer that opened its last stream, sent nothing on it, and waited
  on another open got no stream until it wrote on that stream or two more streams
  ended); a `MAX_STREAMS` only to a peer that sent `STREAMS_BLOCKED`, with that frame
  sent at once (a peer on another QUIC stack, or on the release, waits up to a third of
  the idle timeout for its `STREAMS_BLOCKED` to go); a `streams_max` under 8, so that
  1/8 of it is 0 (a limit that hides the cause); a retry in `transport` (the peer still
  gives no slot). Decided by `laptop.architect-2` (2026-10-09T01:28:23Z:
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6072386060), which
  supersedes the shape of
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6071647041, of
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6071044405, and of
  https://github.com/synnaxlabs/foundation/issues/2018#issuecomment-6070651968.
