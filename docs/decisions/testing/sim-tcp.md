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
  the rejected retransmission. On a link with loss, a connect and its accept can take
  longer than one round trip and one and a half. Each try draws once, at its send, and a
  link with no loss makes no draw. A lost segment other than an ACK or an RST holds the
  later segments of its stream in its direction other than ACKs and RSTs, and goes again
  after its RTO: 1 s for a SYN, a SYN-ACK, or an RST that answers one of them, else the
  delay and jitter of both legs plus 200 ms, twice as long after each loss, to at most
  120 s. A lost ACK or RST holds nothing: the next segment of its stream in its
  direction sent after its loss carries it, and it goes again after its RTO only when
  none arrives first. An ACK that goes takes the place of the lost ACK of its stream and
  direction. A lost ACK that no end waits on has no timer until its end waits, and then
  goes again after its RTO. An RST that answers a segment at a closed port or a refused
  connect goes again as long as that segment would, as its sender sends it again and
  meets the same answer; the RST of an abort never goes again. A connect gives up 127 s
  after its first SYN (7 tries), when its SYN, the SYN-ACK, or the answering RST is lost
  each time, and a SYN-ACK goes at most 6 times. Another segment goes until the first
  try 924.6 s or more after its first send, or after its hold for a held segment, as
  Linux bounds `tcp_retries2` by time. Then the end that waits on it times out with
  `TimedOut` and sends no RST: the sender of a SYN-ACK, bytes, or a FIN, and the
  receiver of an ACK or an RST that waits on one. The client of a SYN-ACK that times out
  does not know that its SYN arrived, and gives up 127 s after its first SYN. When an
  end leaves its pair before its stream closed (an RST, a timeout, a drop, or a power
  cut of its node), the lost answers to it drop, and its peer sees its lost segments as
  a closed port would answer them: a held SYN-ACK and a lost ACK that an end waits on
  each become a lost RST with their tries, and the rest drop. A clean close keeps them,
  as TIME_WAIT does. Not as Linux: no fast retransmit (a lost segment waits for its RTO,
  where Linux sends it again about one round trip after three duplicate ACKs); no
  congestion window; the last ACK of a handshake that is lost each time holds the
  accepting end until 924.6 s with the RTO of data, where Linux sends the SYN-ACK again
  at 1 s and drops the request at 63 s (a program sees no change, as its `accept` gives
  nothing in both); a SYN that arrives does not go again, so a SYN-ACK gets no extra try
  from it; and a lost answering RST goes again in place of the segment that it answers,
  so each try draws loss once where Linux draws for both. Lost: full TCP, which gives
  the same order as this rule but not the same times; smoltcp, whose RTO runs from 1 s
  to 60 s with no limit on tries and which takes IP frames, where Linux, which `sim`
  follows, uses 200 ms, 120 s, 924.6 s, and 127 s; loss as more delay at the send, which
  cannot see a link that heals and never ends at loss 1; one hold for each direction,
  which holds a newer stream of the same pair behind a lost segment of an older one,
  which TCP never orders; and an ACK that waits behind held segments, which stops the
  writer of the other direction for an RTO where Linux stops nothing.
  Plan: https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280717184.
  Plan terms of `laptop.architect-2`, the architect of `sim`:
  https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280756367. The two
  choices past the plan, approved by `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6281231577.
