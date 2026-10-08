- **SIM TCP (2026-10-05)** `sim` models TCP segments on the same links as UDP. A segment
  is never lost or duplicated. It arrives after the delay and a jitter draw of its link,
  and never before an earlier segment in its direction, so each direction keeps its
  order. Each segment carries the key of its stream, and only the end of that stream
  takes it, so a late segment of an older stream on the same pair meets a closed port. A
  connect is ready after one round trip and its accept after one and a half. The receive
  buffer sets the window, the send buffer holds the bytes that the peer has not
  received, and a write waits while `unsent_bytes_max` bytes are not sent. A drop before
  close, or with bytes unread, sends an RST; a drop after close sends the bytes and the
  FIN. A stream is done when an RST arrived, or each FIN arrived and its own is acked. A
  drop of it sends nothing. An end that is done leaves its pair, as a Linux socket
  leaves its table: a segment to the pair then meets a closed port, a SYN opens a new
  stream, and a connect may take its port, also while a driver holds the old end. A
  process crash drops each stream. A power cut sends nothing, so the peer gets an RST
  only when it sends. A case that `sim` does not model panics with "sim does not
  simulate ... yet": a link with loss, `delayed` sends, a connect to an address with no
  node, a full backlog, and a SYN to a live stream. Rejected: retransmission over a
  lossy link (a full TCP state machine to test before a carrier needs it), and a pipe of
  bytes with no segments (no window, so no test of a writer that a slow reader stops).
  Built by `simulation` in #113 and #944. Amended (2026-10-06, #874): a stream that is
  done sends no RST at its drop and leaves its pair.
