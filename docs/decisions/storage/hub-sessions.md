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
  reader, which `Reader` drives, and which `Link::serve` drives (HUB LINK, #1946).
  The split costs `latest next` +1 ns per frame (16 against 17 ns net on a quiet
  host), which adds 0.3% to the write of one frame. Accepted by laptop.architect:
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
  Supersedes the `Hub::define` clause of
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031908575, the patch
  that took a `hub::Channel`. Decided by `laptop.architect` (2026-10-08T18:36:19Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066571400).
  Supersedes item 3 of
  https://github.com/synnaxlabs/foundation/pull/1926#issuecomment-6066382854.
  Changed by #1969: `Hub::define` takes a spec's definitions and skips each that is not
  a channel (`laptop.architect`, 2026-10-08T18:49:47Z:
  https://github.com/synnaxlabs/foundation/issues/1969#issuecomment-6066796714).
  Supersedes the argument of `Hub::define` in
  https://github.com/synnaxlabs/foundation/issues/1917#issuecomment-6064624349.
  Amended (2026-10-08T19:18:09Z, #340): the hub takes the region's mesh
  (`hub::Config::mesh`, `None` for a node with no region), and `hub::Config::node` stays
  the one source of this node's key. `define` never carries an index. A writer, a
  reader, or an open that `Link::serve` gives waits until the mesh names a home for each
  of its indexes. At this node, the first such session carries the index, once: a later
  carry does nothing (`home::Shard::carry`), so the hub keeps no set of carried indexes
  (`laptop.architect`, 2026-10-08T19:30:54Z:
  https://github.com/synnaxlabs/foundation/pull/1979#issuecomment-6067505377). With no
  mesh, this node is the home of each index. No frame comes before a session, so nothing
  waits on the carry. When the home is another node, `writer::Error::Remote` and
  `reader::Error::Remote` give it, and `serve` stops the stream with `NOT_HOME`. A
  stopped mesh gives `Mesh` with why it stopped (code `FAILED` in `serve`). Trigger:
  4d-b of #340 removes `reader::Error::Remote` when the hub reads from another node.
  Decided by `laptop.architect`: the mesh (2026-10-08T18:42:42Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066677536), the
  split and `reader::Error::Remote` (2026-10-08T18:51:16Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066821273), and one
  carry rule (2026-10-08T19:18:09Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6067290747), which
  supersedes "`define` carries at once" in item 2 of
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066677536.
  Amended (2026-10-08T22:24:43Z, #2020): `Hub::set_definitions` replaces `Hub::define`.
  It makes the channels of a spec's definitions the channels that sessions may name. A
  known channel whose key, name, and definition stay keeps its sessions. Each other
  known channel is removed, so a rename at the same key or a changed definition is a
  removal and a new channel. The hub keeps the definition of each channel for this
  compare. Each session on a removed channel ends at once, in the call: a writer gives
  `writer::Failure::Removed` on this and each later write, and the home closes it; a
  reader gives `reader::Ended::Removed` before any frame that waits, and a reader that
  waits in `next` wakes; a served open stops with code `UNKNOWN`
  (`serve::Error::Removed`). Each names the key of the first channel of the session that
  was removed. Each per-call check of a session (`Writer::write`, `Session::take`,
  `Credit::grant`) reads only a cell that the session shares with the hub, never a map,
  so its cost stays the same while other sessions end. The hub finds a session by its
  home key, as the shard never gives a key twice. The home stops carrying an index only
  when its key is not an index of the new definitions (`Shard::shed`, HOME SURFACE), and
  carries it again when it returns. A key holds at most two slots (`channel::Slots`):
  its slot as an index, which never changes, as the buffer keys its tails by slot (X42),
  and its slot as a data channel, which the hub retires at each removal. So a channel
  defined later at a key gets a new slot, and a reader takes no series written under the
  old definition. An index continues its seq after its key was a data channel, also
  after a restart (`laptop.architect`, 2026-10-09T01:25:59Z:
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6072361464). So a slot
  names one definition of a data channel, and the newest frame of an index stays the
  current value of each other channel on it (B4). Lost: a drop of the newest frame of
  each index that a removed channel was on, as each other channel of the index then has
  no current value until the next live frame; a copy of the newest frame without the
  removed series, a new key set and a copy for the same result; and a check of each
  frame in each session, a cost per frame for a change that comes at an apply
  (`laptop.architect`, 2026-10-09T00:41:53Z:
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6071897947). A served
  open checks each key as its message arrives, and is a session on each channel whose
  key it checked, from that check. A removal of one of them ends it at once, in the
  call, with `serve::Error::Removed` of the first channel of the open that a call
  removed, and code `UNKNOWN`: also a rename, a move to another index, another data
  type at the same key, and a removal that a later call undoes. While it waits for the home, the call wakes it, as a removed index can get no
  home. It reads its removal before it checks each later key and after the wait, and
  then carries the index and opens with no `await` between. So its index changes only
  at a removal, and it waits once. A key that a call changes before the open checks it
  is checked against the new definitions. An open by name checks its names again after
  it waits for the home, then carries each index and opens with no `await` between, and
  when the check gives another index, it waits again (`laptop.architect`,
  2026-10-09T05:24:32Z:
  https://github.com/synnaxlabs/foundation/issues/2112#issuecomment-6074871156;
  2026-10-09T00:55:16Z:
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6072038723). The call
  checks the definitions before it changes anything: two channels with one key or one
  name, or a data channel whose index is not an index of the definitions, panic. This
  changes "A known key or name panics", "The hub keeps the key, the sample type, and the
  index of each", and "The PR that defines channels at each new spec decides what a
  known, renamed, or removed channel does" in
  https://github.com/synnaxlabs/foundation/issues/1917#issuecomment-6064624349. Lost: a
  session ends at its next call, under which a writer keeps the control of a removed
  index until it calls, and a reader that waits in `next` needs a wake anyway; and the
  home ends the sessions in `shed`, which knows no data channel, so a removed data
  channel needs a second path; and a hub number for each session, a second key for the
  same fact. Decided by `laptop.architect` (2026-10-08T22:24:43Z:
  https://github.com/synnaxlabs/foundation/issues/2020#issuecomment-6070259814;
  2026-10-08T22:09:38Z:
  https://github.com/synnaxlabs/foundation/issues/1957#issuecomment-6070012949).
