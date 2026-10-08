- **HOME CLOCKS (#191)** A shard reads monotonic time and mesh time itself, from the
  `clock::Reader` in its `Config`, in each call that needs them. One
  `clock::Reader::now` gives both at one instant, so a control lease and a stamp check
  in one call see the same time, and a lease never compares readings of two clocks
  (approved by the coordinator on 2026-10-06, #964). Before the node first has mesh
  time, it opens no writer, with `writer::Error::Unsynced`. A write needs an open
  writer, so it never meets that case. An unnamed reader opens with no mesh time: its
  open and its close take no stamp (#1024; decided by the architect, #963). A named
  reader needs mesh time (HOME NAMED READERS). This is a patch: #523
  decides where samples wait before the first estimate (CLOCK PEER ANSWER), and removes
  or keeps `Unsynced`. Lost: time as arguments of each call, because each caller repeats
  the same two reads and can pass an old one. Approved by the coordinator on 2026-10-05
  (#191). Mesh time in the home (the ahead limit and the stamp of each entry) is the
  midpoint of the mesh time of `clock::Reader::now`, which never goes back. Lost: the
  latest edge, because it goes back when the error shrinks, and with an unknown error
  (OS CLOCK BOUND) it is 36500 days ahead, so the ahead limit stops nothing and one bad
  stamp makes each later true stamp `Backwards` (#952 review, 2026-10-06).
