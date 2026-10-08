- **CREDIT RULES (write-path, advisor, and data-path, 2026-10-05)** A complete reader's
  `hub` grants credit to each session on one index as an absolute byte limit since the
  session opened, in a `Credit` message apart from the ack. Both sides count from zero
  at each session, including a takeover and a resume at a new home. The open carries the
  first grant; until then the session has no credit. A grant only raises the limit, so a
  repeated or reordered grant does no harm. A `Credit` is sent reliably: a blocked
  session gets no frame, so no later grant would replace a lost one. The home drops a
  grant for a session it closed. The home sends a whole frame while the bytes it has
  spent are below the limit, so it passes the limit by less than one frame and never
  splits a frame. A frame that finds the limit spent waits at the home, and so does each
  later frame; the session takes it while the bytes it spent are below a later grant. A
  frame that still waits when the home releases the next commit with frames of its index
  is refused. A grant wakes no session: the task that grants takes after it. So a reader
  that takes the frames of each commit before the next such commit ends is never
  refused, whatever the size of the commit. laptop.architect decided this on
  2026-10-08T07:23:30Z:
  https://github.com/synnaxlabs/foundation/issues/1170#issuecomment-6054827276 (#1170).
  After a refusal, the session gets no later frame until it has the refused one; frames
  from catch-up spend credit too. A frame costs its charge, `Frame::charge`: the bytes a
  block of the frame's length takes from a pool. That is `block`'s header plus the whole
  frame (M3), rounded up to its size class, so a frame costs its length plus the header
  and at most 64 bytes or a quarter of its length more, and a frame with only empty
  series still costs its headers. The charge depends only on the frame's length, so the
  home and the `hub` compute the same charge for the same frame. A remote complete
  reader gets only the series of its view (M2): the home sends a frame of those series
  in the reader's entry order (HUB WIRE), and both ends charge that frame. The person
  chose this on 2026-10-05 ("B is approved ... send only partial frames"), #267. Each
  complete session has a `delivery::complete::Charge`: `Whole` (a local reader) spends
  the home's frame, and `Places` (a remote reader) spends the frame of one series for
  each slot it lists that the frame holds, in listing order, the first listing of a slot
  only. Catch-up uses the same `Charge`. So a remote session pins home blocks up to its
  window times the ratio of the home's frame to its view. laptop.architect decided this
  on 2026-10-07T22:47:54Z:
  https://github.com/synnaxlabs/foundation/issues/1642#issuecomment-6048424611.
  `home::reader::complete::Charge` re-exports it, and `home::Shard::open_complete` takes
  it, so `hub` does not depend on `delivery`. The `Charge` adds about 7 ns per frame to
  `release` with one `Whole` session; that is accepted, with the `Places` state boxed,
  so that a `Whole` session grows by one pointer and not by the size of `Places`.
  laptop.architect decided both on 2026-10-07T23:13:16Z:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6048741570. The box2
  rerun gave 8.0 ns per frame at one session and +4.0% at 16; laptop.architect ruled
  on 2026-10-07T23:38:44Z that the acceptance covers it:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6049045510. The
  `hash::Map` of those states adds about 0.3 ns per place to `release` at 100k places
  (+10%); laptop.architect accepted it on 2026-10-07T23:32:22Z:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6048971185.
  `types::frame::Places` holds this charge and the layout of the frame that `serve`
  sends (HUB WIRE, #1648), with laptop.architect's OK on 2026-10-07T23:22:03Z to move
  it out of #1655:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048849864, and
  its surface approved on 2026-10-07T23:34:09Z:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122. The
  charge is part of the wire contract: a change to `block`'s header or size classes
  needs a new wire version (C9d). The classes changed to four per
  doubling under wire version 1 (#188), because no release carries that version. The
  window counts charges, not wire bytes. Per-connection framing in `wire` (X35) pins no
  pool memory and does not count. Credits apply only to complete delivery, which is
  reliable: a lost frame would leak credit. The `hub` raises the limit only after it
  releases a frame, and it bounds its decoded copies itself, since a small encoded frame
  can decode to much more. It sends a `Credit` only when the room it has not announced
  reaches half the window, and puts the grants for all sessions on one link into one
  message. It sizes one window per reader from the link's bandwidth-delay product,
  adapts it, and divides it among the indexes the reader reads. Each session with room
  can pass its limit by one frame, so the `hub` counts one largest frame per such
  session against the window, and a reader pins at most its window, plus the frames of
  the last release of each index it reads that wait for it. The sessions of an index
  share those frames, which memory held until that release; on a quiet index they stay
  until each session takes them or closes. Replaces r11 5.2 (a window beyond the
  acknowledged position): flow control stays apart from durable acks. Basis: B3, M3,
  MEMORY BOUNDS, X35, r11 5.2, #41, #267.
