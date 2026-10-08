- **CLOCK PEER ANSWER (2026-10-05)** A node with no mesh time answers a peer with its OS
  reading and its OS bound. Cold nodes then vote with each other's OS clocks, and each
  waits until more than half agree (ESTIMATE COMBINE). An answer with an unknown bound
  (an unknown estimate, or an OS clock with no bound) says "unknown" and carries its
  offset. The asking node measures it as `exchange::Reading::Unknown`, so the answer
  cannot narrow into a known bound (#930). A node answers from one read of its clock,
  sent as both intervals, because two reads can straddle a sync and pair a known
  interval with an unknown one. The read is after the request arrived and before the
  answer left, so it bounds both ends of the exchange. An unknown answer carries
  `Measurement::time`, because the midpoint of the interval moves after 2162 (#145). A
  peer that never answers counts against a majority, and `node` removes no source. Lost:
  a node with no time does not answer, because then a mesh that starts cold never syncs;
  an answer of "no time" that takes the source out of the vote, because a node with a
  bad OS clock then syncs on itself; `node` removes a silent source after a timeout, a
  patch that puts time policy in layer 4. The person decided on 2026-10-05 ("Yeah that's
  fine"), #145. So a node that starts while no peer answers stays unsynced, even with a
  good OS bound. Its samples keep their local monotonic reading, and the node stamps
  them in mesh time when the first estimate comes, with the error of that estimate at
  each reading (200 ppm: 0.72 s after 1 h). The buffer holds the samples until then, and
  a node that never syncs fills it. Lost: drop the samples, a patch that loses data;
  stamp them with OS time at once, a patch that writes a time the clock refused and
  cannot correct later. The person decided on 2026-10-05 ("(b)"), #145.
  `clock::Reader::first` gives that stamp: the first estimate at a reading. Later
  estimates never change it, so the stamps keep the order of their readings and are
  never after mesh time (#523). `clock::Reader::now` gives a `clock::Time`: a reading
  of the monotonic clock, and mesh time at that reading, from one read of the clock, so
  a sample with no mesh time keeps that reading. Lost: mesh time at a reading the
  caller made, which can go back while the clock slews down. Approved by the
  coordinator on 2026-10-06 (#964).
