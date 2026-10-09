- **HUB WIRE (#561)** A remote reader session is one hub stream of class `Complete` or
  `Latest`. After the header, the reader's node sends `wire::hub::Open`: the mode and
  the number of channels, all on one index. A latest session gets the newest live frame
  before its commit. A complete session gets each live frame after its commit, and
  `Open` carries its first grant in bytes (CREDIT RULES). The home answers `Opened`; the
  reader sends `Credit`, its total grant since the open; the home sends each frame as a
  `Head` (path, seq, count, and the number of series). Every frame is encoded (X35), so
  `Head` has no form. The body holds only the series of the reader's view, the index
  series too, written from the frame's block as slices, and both ends charge the frame
  that the reader builds (CREDIT RULES, M2). `serve` opens each complete session with
  `delivery::complete::Charge::Places` of the slots of its open (#1642, #1636; the
  architect, 2026-10-07T22:25:18Z,
  https://github.com/synnaxlabs/foundation/pull/1636#issuecomment-6048108229). A series
  has the place of its first listing in the open, from 0. The reader's `hub` lists the
  keys in the entry order of its own frame (its slot order), the index too, so a place
  is an entry of the reader's frame and a session has `channels` places. An open whose
  keys do not hold the index is not valid: the home's `hub` checks it and stops the
  session with `MALFORMED` (lost: `UNKNOWN`; the architect, 2026-10-07,
  https://github.com/synnaxlabs/foundation/pull/1236#issuecomment-6032902101). An open
  of no channel is not valid. Only the fixed part of `Open` and of `Head` is one
  message. The rest is one run of bytes, in messages of at most the peer's
  `message_bytes_max`, back to back with no prefix: after `Open`, the keys; after
  `Head`, the place and end of each series in the body, then the body. A message never
  splits a key or an end, so each side decodes each message as it arrives. The keys run
  holds exactly `channels` keys and the ends run exactly the head's number of series, so
  each side counts them to find where a run ends, and the body starts a new message. So
  no count of channels or series has a cap, and the reader fills one block of its
  frame's length: the header, the range, a descriptor for each series, and the body to
  the last end. A run message with more keys or ends than remain is not valid. A head of
  no series is not valid, since a frame holds its index. The home checks each key as it
  arrives and never allocates by the peer's count. A head with more series than places,
  or an end with a place the session does not have or that is not above the place before
  it, is not valid; `wire::hub::Reader` checks the head as it arrives and `types` checks
  the ends, so the reader holds no more ends than it has places. The ends and the body
  are in place order: the home writes the series of each place it has, from 0, each from
  the frame's block as a slice, with ends it computes in that order. It cuts each series
  from `Frame::body` by `frame::Places::lay`, which finds them with `View::bounds`
  (the architect, 2026-10-07T22:35:41Z,
  https://github.com/synnaxlabs/foundation/issues/1639#issuecomment-6048265226; lost:
  `View::ends`, which gives no start, and `Frame::bounds`, a search for each place).
  `View::bounds` is crate-private, as no crate outside `types` calls it (the architect,
  2026-10-08T00:49:22Z,
  https://github.com/synnaxlabs/foundation/pull/1668#issuecomment-6049855032; lost: a
  public `bounds`, a second way to lay a reader's frame beside `Places`). `serve`
  writes each ends message from the series that `Places::lay` gives, in place order,
  with `wire::hub::ends::encode`, which sizes the message by its buffer, so no scratch
  buffer holds the ends (the architect, #1146,
  https://github.com/synnaxlabs/foundation/issues/1146#issuecomment-6032284157). It
  takes exactly the ends the buffer holds and no more, so one iterator passed with
  `by_ref()` splits a run into messages; the caller owns the count of the run (the
  architect,
  https://github.com/synnaxlabs/foundation/pull/1258#issuecomment-6033667563). The first
  series starts at 0, and each other at the end before it rounded up to a multiple of 8.
  So the body is the series bytes of the reader's own frame (FRAME LAYOUT), and the
  reader builds that frame in one block: the header and descriptors that `types` writes,
  then the body as it arrives, with no copy of a series after the receive. An end below
  the start of its series is not valid; `types` refuses it, as `frame::check` does. The
  padding may hold any bytes (FRAME LAYOUT). Each direction has its own messages: the
  reader sends `Open`, then `Credit`; the home sends a `Reply`, `Opened`, `Head`, or
  `Behind`. The home sends `Behind` after the last frame before a miss of the session,
  then finishes its stream. The reader's `next` gives each frame before it, then
  `Ended::Behind`. `hub` builds both in #340 PR 4. A message after `Behind` is not
  valid (lost: a stop code, which can cut off the frames sent before it; the architect,
  2026-10-07T21:07:50Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6046877541).
  `Behind` and `Credit` in a latest session are not valid (lost: accept them in either
  mode, which lets a remote latest reader give `Ended::Behind`; the architect,
  2026-10-07T21:34:58Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047310321). The
  check of the mode costs the decode of a `Credit` +0.28 ns. The `Ended` state and the
  mode flag of `Reader`, for `Behind`, cost a body message up to +0.37 ns and a frame up
  to +1 ns. Both are accepted with no code change; a `Credit` decode past +1 ns over
  `main` comes back to the architect (lost: `#[inline]` on `wire::hub::Home::decode`,
  which is not measured and grows each caller; the architect, 2026-10-07T22:18:40Z,
  https://github.com/synnaxlabs/foundation/pull/1631#issuecomment-6048002213, and
  2026-10-08T00:16:49Z,
  https://github.com/synnaxlabs/foundation/pull/1631#issuecomment-6049484466). A
  latest open needs a stream of class `Latest`, and a complete open a stream of class
  `Complete`, since the class sets the priority of each frame that the home sends back;
  the home's `hub` checks it at the `Open` and stops the session with `MALFORMED`.
  Stop codes: 16 `UNKNOWN` (a channel the home does not know), 17 `NOT_HOME` (the node
  is not the home of the index), 18 `FAILED` (the home failed: its buffer or its mesh
  stopped), 19 `BUSY` (the side that stops had no block for the session, in both
  directions; a later open can succeed, but not when the block is larger than each
  block of that side's pool), and 2 `wire::header::MALFORMED` (a message that does not
  decode, comes from the wrong side, or breaks a rule above), which every protocol may
  use. The meanings of 18 and 19 were decided by the architect (2026-10-07T23:31:29Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6048960511). The
  exception for a block larger than each block of the pool was decided by
  `laptop.architect` (2026-10-08T23:44:54Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6071260886).
  Supersedes the meanings of 18 and 19 in
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047300641 ("the
  home's buffer failed") and
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047519084 ("the
  home had no memory for a reply"), and the meaning of 19 in item 3 of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6069496483 ("the node
  had no memory for a reply or a request body"). #2012 replaces the meaning of 19 with
  one text for both causes
  (https://github.com/synnaxlabs/foundation/issues/2012#issuecomment-6071577074). Until
  then, the meaning of 19 is the one this record gives above, from 6071260886
  (`laptop.architect`, 2026-10-09T00:13:01Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6071579227). A reset
  drops the frames in flight, which is correct for `FAILED`, since the session cannot
  go on (lost: a `Reply::Failed` that keeps them, a second end message to fuzz). Each
  reply block holds one message. An ends message holds at most the frame's series, at
  8 bytes each, the size of their descriptors in the frame's block, so the pool can
  always hold it (the architect, 2026-10-07T22:17:44Z,
  https://github.com/synnaxlabs/foundation/pull/1636#issuecomment-6047985988).
  Supersedes "A reply block holds at most `min(bytes_max, Pool::largest)` bytes"
  (https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047519084). The
  home ends the session on `BUSY` and does not wait: the pool gives no wake, so a wait
  needs a clock in `hub` and a wait queue for each session, and the end frees the
  frames the session pins (`mesh` ends its stream in the same case). The class rule and
  code 18 were decided by the architect (2026-10-07T21:34:19Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047300641), code 19
  by the architect (2026-10-07T21:47:56Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047519084).
  Lost: a `message_bytes_max` of at least the largest pool block (a client or a
  foreign peer can set 1472, and it ties `transport` to the pool); a cap of 91 channels
  a session, the most that fit in 1472 bytes; the index in its own field of `Open`,
  because the home knows its index and a second copy needs a check; the whole
  `Frame::body` (a reader gets only its view); an `UNSYNCED` code, because an unnamed
  open needs no mesh time (READER RULES), and a later named open can add one; grants for
  many sessions in one message, which wait until a link carries a second session; a
  public series count on `View`, for a home that writes the ends from `View::iter`,
  which gives the home's entry order and not place order (the architect,
  https://github.com/synnaxlabs/foundation/issues/1146#issuecomment-6032284157). The
  coordinator approved the messages (2026-10-05); the architect decided the rest (#561,
  2026-10-06) and the run, the index place, and `MALFORMED` on #1064
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030652085), then
  whole keys and ends and one message type for each direction
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030699163), then the
  open of no channel and the place checks in `hub`
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030906615). Amended
  (2026-10-07, #1068): the body follows the places, not the home's entry order, so an
  end whose place is not above the place before it is not valid; both ends charge the
  reader's frame. Lost: a copy of each series at the reader (one per sample at every
  remote reader at P1 rates, which the home's free order cannot justify,
  `docs/claude/performance.md` rule 10); a reader key set in the home's order (key sets
  are sorted by slot); a start in each descriptor (a disk and wire format change, C9d).
  Decided by the architect, #1068
  (https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6031655359). The
  byte form, little-endian: `Open` is kind 1 (latest) or 2 (complete, then `limit_bytes`
  `u64`), then `channels` `u32`; `Credit` is kind 3, then `limit_bytes` `u64`; `Reply`
  is kind 1 (opened), 2 (head: path `u8`, live 0 and backfill 1, seq `u64`, count
  `u32`, series `u32`), or 3 (behind, no fields, by the Behind rule: the architect,
  2026-10-07T21:07:50Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6046877541); a key is
  a `u128`; an end is place and end, each `u32`. Amended (2026-10-07, #1196): the
  message order, the runs, and the head bound move from `hub` to two stateful decoders
  in `wire`, `hub::Home` at the home and `hub::Reader` at the reader's node, each with
  an exact error for each broken rule, so `hub` checks no wire rule. Decided by the
  architect
  (https://github.com/synnaxlabs/foundation/issues/1196#issuecomment-6032630529).
  Amended (2026-10-07T14:56:48Z, #1455): `Reader::decode` checks a message in three
  steps and gives the error of the first that fails: the bytes (its decode error), the
  order of the session (`Unopened` or `Reopen`, whatever the content), then the content
  against the session (`Places`, `Run`, `Body`). A head before `Opened` is not a head
  of this session yet, so its series count has no session to break. Lost: `Places`
  first. Decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/1455#issuecomment-6040654132).
  Amended (2026-10-07T23:34:09Z, #1648): `types::frame::Places` holds the layout of a
  remote reader's frame for `delivery` and `serve`. `Places::lay` gives each series in
  place order, with its bounds in the home's `Frame::body` and its end in the reader's
  frame; `Places::charge` is the `Frame::charge` of that frame, in O(1) when the places
  name each entry of the key set, in any order: each block payload is a multiple of 8
  bytes, so the padding of the last series does not change the footprint (the architect,
  2026-10-08T00:25:50Z,
  https://github.com/synnaxlabs/foundation/pull/1668#issuecomment-6049589885. Supersedes
  "in entry order" in
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122). Lost: a
  free function that lays one frame, with each caller keeping its own state for each key
  set, so `delivery` and `serve` each repeat it. Also lost: one `Places` for each remote
  session, whose layout `release` keeps with each frame for `serve`: each frame in the
  queue would hold its layout. So a remote session holds two. Decided by
  laptop.architect:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122.
  Supersedes: "At the open it makes the list of each place and its home entry, sorted by
  place" above; `Places` makes it at the first frame of each key set. `lay` walks the
  places for a frame with at least one series at the places for each 8 entries that
  they name, and sorts the series of a sparser frame. Lost: walk only (10 series of 100k
  places took 140 to 420 µs, not 0.5 to 0.7 µs), and sort only (a scattered frame of
  100k series took 3.9 to 6.0 ms, not 1.6 to 1.7 ms). The architect accepted the cost of
  the dense walk against 1e658b7a, up to the head numbers of #1695 (laptop.architect,
  2026-10-08T01:36:39Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6050376022). The cut
  counts only the series at the places, and is 8. Lost: a count of each series of the
  frame (`outside_lay` 56 to 69 µs, not 0.1 µs), and a cut of 16 (a frame just over it
  cost 148 to 255 µs more than one just under). The architect also accepted
  `narrow_lay` at +3 to +4 ns per frame (laptop.architect, 2026-10-08T02:33:03Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6050982066). Each
  dense frame first pushes ceil(m/8) series, for the m entries that the places name,
  then moves them into the walk: the architect accepted +5.6% at `cut_lay` 12,500 and
  +1.3% at `reversed_lay` 100k for -21.7% at `cut_lay` 12,499 (laptop.architect,
  2026-10-08T03:07:31Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6051347440).
  Amended (#1631): after `Behind`, each message gives `Ended`, before the three steps
  and whatever its bytes, since the home sends nothing after `Behind`. Step 3 also
  gives `Latest` for a `Behind` in a latest session, since only a complete session
  falls behind. Decided by the architect (2026-10-08T01:04:51Z):
  https://github.com/synnaxlabs/foundation/issues/1689#issuecomment-6050026992.
  At the reader's node (#340 PR 4d-b), a message that `wire::hub::Reader` refuses, or
  ends that `types` refuses, stops the stream with `MALFORMED`, and a pool with no
  block for `Open`, its keys, a frame, or a `Credit` stops it with `BUSY`. Each later
  `next` gives the same `Ended`. Decided by `laptop.architect` (2026-10-07T23:31:29Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6048960511). A
  `Credit` that still waits to send when the stream stops drops with its sender, so the
  home's receive half resets with code 0, not the refusal code, until #2031
  (`laptop.architect`, 2026-10-08T23:49:49Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6071319399).
