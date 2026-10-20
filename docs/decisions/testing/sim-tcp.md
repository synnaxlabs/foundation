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
  `sim` sends it again. This replaces "never lost", the panic on a link with loss, the
  rejected retransmission, and "the peer gets an RST only when it sends". On a link with
  loss, a connect and its accept can take longer than one round trip and one and a half.
  Each try draws once, at its send, and a try of a lost answer twice, as below. A link
  with no loss makes no draw. A lost segment other than an ACK or an RST holds the later
  segments of its stream in its direction other than ACKs and RSTs, and goes again after
  its RTO: 1 s for a SYN or a SYN-ACK, else the delay and jitter of both legs plus
  200 ms, twice as long after each loss, to at most 120 s. A lost ACK or RST (an answer)
  holds nothing. The next segment of its stream in its direction sent after a lost ACK
  carries it, and the ACK goes again after its RTO only when none arrives first. An
  answer that goes takes the place of the lost answer of its stream and direction. A
  lost answer in place of one with a timer keeps that timer, and the count and the
  924.6 s of the one of the two that began first. A lost answer has the RTO and the
  limit of the end that gets it at each try: of a SYN to an end that connects, of a
  SYN-ACK to an end that accepts, and of bytes to another end. It goes again only while
  that end waits, and else has no timer until the end waits, and then goes after its RTO
  with a new count. It goes as the try of that end that meets it would: it is lost as
  the loss of the link from that end draws, and else as the loss of its own link draws.
  An ACK of a segment, and an RST that answers a segment at a closed port or a refusal,
  continue the count and the 924.6 s of that segment. A lost RST to an end that
  connects, due while a listener receives at its source, goes as the next SYN of that
  end with its count, and the listener takes it. A connect gives up 127 s after its
  first SYN (7 tries), when its SYN, the SYN-ACK, or the answering RST is lost each
  time, and a SYN-ACK goes at most 6 times. Another segment goes until the first try
  924.6 s or more after its first send, or after its hold for a held segment, as Linux
  bounds `tcp_retries2` by time. Then the end that waits on it times out with `TimedOut`
  and sends no RST: the sender of a SYN-ACK, bytes, or a FIN, and the receiver of an ACK
  or an RST that waits on one. The client of a SYN-ACK that times out does not know that
  its SYN arrived, and gives up 127 s after its first SYN. When an end leaves its pair
  before its stream closed (a timeout, a drop, or a power cut of its node), its held
  segments drop, and its peer gets one RST: its lost answer or its held SYN-ACK as an
  RST, with its next try and its count, or else a new RST. The RST of an abort (a drop
  of an open end before its close or with bytes unread, or a listener that fails or
  drops) goes at once. The RST of bytes to an orphan goes at once and continues the
  count and the 924.6 s of the bytes. The RST of a timeout or a power cut is lost, as
  the peer meets the closed port only when a try of it arrives there: a peer that only
  reads waits forever, as on Linux with no keepalive, and a peer that cannot reach the
  node times out. A lost answer to an end that left drops. An end that leaves at an RST
  only drops its held segments, as no peer is left. A peer that connects gets none while
  a listener receives at its port, and gives up 127 s after its first SYN. A clean close
  keeps the lost segments of the end, as TIME_WAIT does. After a power cut, the lost
  segments from the node other than RSTs drop. Not as Linux: no fast retransmit (a lost
  segment waits for its RTO, where Linux sends it again about one round trip after three
  duplicate ACKs); no congestion window; a SYN that arrives does not go again, so a
  SYN-ACK gets no extra try from it, and a connect whose accepting end leaves while a
  listener receives at the port gives up at 127 s, where its next SYN on Linux gets a
  new SYN-ACK; a lost answer goes again at the RTO of the end that gets it, not one
  flight after the try that meets it; the peer of a node whose power is cut gets its RST
  also while the node is down; and no zero-window probe: a writer that waits for room
  meets a closed peer only through that lost RST.
  Lost: full TCP, which gives the same order as this rule but not the same times;
  smoltcp, whose RTO runs from 1 s to 60 s with no limit on tries and which takes IP
  frames, where Linux, which `sim` follows, uses 200 ms, 120 s, 924.6 s, and 127 s; loss
  as more delay at the send, which cannot see a link that heals and never ends at loss
  1; one hold for each direction, which holds a newer stream of the same pair behind a
  lost segment of an older one, which TCP never orders; an ACK that waits behind held
  segments, which stops the writer of the other direction for an RTO where Linux stops
  nothing; and a class fixed at the loss of an answer, which times out an accepted end
  at 63 s and never times out a connected one.
  Plan: https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280717184
  (2026-10-20T00:51:55Z). Plan terms of `laptop.architect-2`, the architect of `sim`:
  https://github.com/synnaxlabs/foundation/issues/3040#issuecomment-6280756367
  (2026-10-20T00:54:32Z). The two choices past the plan, approved by
  `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6281231577
  (2026-10-20T01:33:00Z). The lost RST to the peer of an end that leaves, and the tries
  of a lost RST by the end that gets it, by `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6282180756
  (2026-10-20T02:49:42Z). The text of the lost RST of an abort and of a lost ACK, by
  `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6282318156
  (2026-10-20T03:01:06Z). The class at each try, the leg from the end that gets an
  answer, and one RST for each leave, from review round 2:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6283286960
  (2026-10-20T04:19:26Z), approved with its texts by `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6283389827
  (2026-10-20T04:28:21Z). The fixes of review round 3, approved with two word fixes
  by `laptop.architect-2`:
  https://github.com/synnaxlabs/foundation/pull/3475#issuecomment-6283869366
  (2026-10-20T05:09:13Z).
