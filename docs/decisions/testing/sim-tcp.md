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
  Amended (2026-10-20, #3040): a segment is lost as the loss of its link draws, and
  `sim` sends it again. This replaces "never lost", the panic on a link with loss, and
  the rejected retransmission. Each try draws once, at its send, and a link with no loss
  makes no draw. A lost segment other than an ACK holds the later segments of its stream
  in its direction other than ACKs, and goes again after its RTO: 1 s for a SYN or a
  SYN-ACK, else the delay and jitter of both legs plus 200 ms, twice as long after each
  loss, to at most 120 s. A lost ACK holds nothing: the next segment of its stream in
  its direction sent after its loss carries it, and it goes again after its RTO only
  when none arrives first. A connect gives up 127 s after its first SYN (7 tries), and a
  SYN-ACK goes at most 6 times. Another segment goes until the first try 924.6 s or more
  after its first send, or after its hold for a held segment, as Linux bounds
  `tcp_retries2` by time. Then the end that waits on it times out with `TimedOut` and
  sends no RST: the sender of a SYN, a SYN-ACK, bytes, or a FIN, and the receiver of an
  ACK that waits on one. The client of a SYN-ACK that times out does not know that its
  SYN arrived, and gives up 127 s after its first SYN. A lost RST never goes again. An
  RST, a power cut, and the drop of an end that did not close drop the lost segments of
  its stream; a clean close keeps them, as TIME_WAIT does. Not as Linux: no fast
  retransmit (a lost segment waits for its RTO, where Linux sends it again about one
  round trip after three duplicate ACKs); no congestion window; the last ACK of a
  handshake that is lost each time holds the accepting end until 924.6 s with the RTO
  of data, where Linux sends the SYN-ACK again at 1 s and drops the request at 63 s (a
  program sees no change, as its `accept` gives nothing in both); and a SYN that
  arrives does not go again, so a SYN-ACK gets no extra try from it. Lost: full TCP,
  which gives the same order as this rule but not the same times; loss as more delay at
  the send, which cannot see a link that heals and never ends at loss 1; one hold for
  each direction, which holds a newer stream of the same pair behind a lost segment of
  an older one, which TCP never orders; and an ACK that waits behind held segments,
  which stops the writer of the other direction for an RTO where Linux stops nothing.
  Plan: https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280717184.
  Plan terms of `laptop.architect-2`, the architect of `sim`:
  https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280756367. The two
  choices past the plan, approved by `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6281231577.
