- **HUB SESSIONS (#1133)** `hub::reader::Reader::next` yields once after 128 frames in a
  row: it wakes its own task and returns `Pending`. So it yields under `sim` as under
  `os`, and `hub` does not depend on Tokio. Lost: the Tokio coop budget, which does
  nothing outside a Tokio runtime. A complete session that misses a frame (one that
  still waits for credit when the next commit with frames of its index is released,
  CREDIT RULES) gets no later frame, as there is no catch-up from the buffer yet. The
  director chose that `delivery` reports the miss and wakes the session, and that `next`
  then ends with an error (2026-10-07T06:01:39Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032004737). So
  `delivery::Readers::release` also names a session that missed a frame and has none
  waiting, and `Readers::take` gives `Next::Behind` after the frames before the miss.
  `home::Shard::take` gives `Next::Behind` until #274. `next` gives a waiting frame,
  then `Ended::Behind`, then `Ended::Buffer`. Lost: an error from `take`, which every
  caller, latest readers too, then handles; a `behind` list beside the woken keys, a
  second list to drain for an event that happens once per session. A `delivery` model
  property test and a 32-run `sim` test stand in for loom and shuttle: the wake never
  crosses a thread. Decided by `laptop.architect` (2026-10-07T06:36:57Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032442901). `take`
  gives `delivery::Next` (`Frame`, `Empty`, or `Behind`), and `Readers::behind` and
  `Shard::behind` go. Supersedes
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032442901 in its lost
  design "an error from `take`": a third case is not an error, each caller of `take`
  uses one path for both modes, and #274 may give a gap from `take`. Lost: `woken` names
  a reader that is behind in a second list, which keeps the two steps and moves the
  state into the hub. Decided by `laptop.architect` (2026-10-08T01:43:19Z:
  https://github.com/synnaxlabs/foundation/issues/1718#issuecomment-6050444671). A
  waiting hub reader has given back every frame, as it grants at each `next` call, so no
  hub test reaches the wake of a session that missed a frame with none waiting. The
  `delivery` tests and the home `sim` test
  `names_a_complete_reader_once_when_it_misses_a_frame_with_none_waiting` reach it, and
  `does_not_name_a_complete_reader_that_misses_a_frame_while_one_waits` checks that a
  miss while a frame waits gives no wake. The hub `sim` test
  `gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window` checks that
  one commit of about three windows wakes a waiting reader, which gets each frame, with
  no hang. The hub `sim` test
  `ends_a_complete_reader_after_its_waiting_frames_when_it_misses_a_frame` checks that a
  reader that takes no frame until a second commit past its window ends gets each frame
  before the miss, then `Ended::Behind`. Decided by `laptop.architect`
  (2026-10-08T07:23:30Z:
  https://github.com/synnaxlabs/foundation/issues/1170#issuecomment-6054827276).
  Supersedes https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6033084119.
  After a warmup, a write and `next` make no heap allocation, while frames wait, while a
  reader waits, and in the write that wakes a latest reader, which a counting allocator
  test binary checks (COUNTING ALLOCATOR); it does not count the commit task. `next`
  gives a `types::frame::View` of the reader's channels and their index (M2), never the
  frame. The view borrows the reader, which releases the frame at the next call, not at
  its first poll, and grants credit for it there (CREDIT RULES): `next` is a plain `fn`
  that returns a future. A caller that keeps data copies it. A session that ends gives
  `reader::Ended`. Decided by `laptop.architect` (2026-10-07T05:53:24Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031908575;
  2026-10-07T05:57:18Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031955051; and
  2026-10-07T07:12:40Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032912929). The
  surface was approved by `laptop.architect` (2026-10-07T14:53:11Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6040585795).
  Amended (2026-10-08T00:13:26Z, #1625): `reader::Session` is the home's side of a
  reader, which `Reader` drives, and which `Hub::serve` (#1636) will drive. The split
  costs `latest next` +1 ns per frame (16 against 17 ns net on a quiet host), which
  adds 0.3% to the write of one frame. Accepted by laptop.architect:
  https://github.com/synnaxlabs/foundation/pull/1625#issuecomment-6049444882.
  A doc states what is true at its commit: `Reader` states no credit window, as a
  latest reader has none, and `Session` names `Reader` and each stream of a remote
  reader as its drivers, since #1636 adds the second (laptop.architect,
  2026-10-08T01:01:26Z,
  https://github.com/synnaxlabs/foundation/pull/1625#issuecomment-6049988923).
  Amended (2026-10-08T16:41:41Z, #1917): `Hub::define` takes each channel of a spec as
  a name and a `spec::channel::Channel` in one call, and defines the indexes first, so a
  data channel may come before its index. A known key or name panics. The hub keeps the
  key, the sample type, and the index of each, and reads no quality, error, or control
  edge: the issue that first serves one of these edges reads it in `define`. The PR
  that defines channels at each new spec decides what a known, renamed, or removed
  channel does. Decided by `laptop.architect` (2026-10-08T16:41:41Z:
  https://github.com/synnaxlabs/foundation/issues/1917#issuecomment-6064624349).
  Supersedes https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031908575
  in `Hub::define` and `hub::Channel`.
