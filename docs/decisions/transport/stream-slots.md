- **STREAM SLOTS (#2018, 2026-10-08)** A node gives the peer each freed stream at once
  while the peer has opened each stream of the last limit that the node sent, whatever
  the peer sends. A slot is one stream of the count `streams_max` that `MAX_STREAMS`
  gives, not bytes of stream data. A stream counts as opened once its first frame
  arrives. The local patch of `noq-proto` 1.3.0 (`docs/dependencies.md`) queues
  `MAX_STREAMS` when the limit can rise by one stream and the peer is at the last sent
  limit, or when more than 1/8 of the window is free, as the release does. It also
  queues it when the stream frees because the node stops it. A stream that frees
  before the packet that opens the peer's last stream gets its frame when that packet
  arrives. With the release alone and `streams_max` 16, a `Session::open` that waited
  got no stream until three streams ended. Cost: below the limit, the frames of the
  release. At the limit, a lone frame for each freed stream, which the peer acks;
  `MAX_STREAMS` carries only the latest limit, so a burst of freed streams costs one
  frame per packet. Lost: a `MAX_STREAMS` at each freed stream, at any count (shape
  (b): the frame goes alone in a new packet, as the ACK that frees a two-way
  stream needs no ACK, so one request and reply cost 2 datagrams each way, where the
  release sends 1); a `MAX_STREAMS` only to a peer that sent `STREAMS_BLOCKED`, with
  that frame sent at once (a peer on another QUIC stack, or on the release, waits up
  to a third of the idle timeout for its `STREAMS_BLOCKED` to go); a `streams_max`
  under 8, so that 1/8 of it is 0 (a limit that hides the cause); a retry in
  `transport` (the peer still gives no slot). Decided by `laptop.architect-2`
  (2026-10-09T00:18:56Z:
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6071647041), which
  supersedes the shape of
  https://github.com/synnaxlabs/foundation/pull/2029#issuecomment-6071044405 and of
  https://github.com/synnaxlabs/foundation/issues/2018#issuecomment-6070651968.
