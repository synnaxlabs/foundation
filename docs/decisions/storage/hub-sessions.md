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
  waiting local hub reader has given back every frame, as it grants at each `next` call,
  so it never waits at a miss with none waiting. A served session gets credit only from
  its peer's grants, so its serve task waits at such a miss when the peer sends none.
  Three hub `serve` tests reach the wake of that miss:
  `sends_behind_and_finishes_when_a_complete_session_misses_a_frame`,
  `charges_a_complete_session_by_the_frame_the_peer_builds`, and
  `sends_each_frame_before_a_miss_then_behind`. The `delivery` tests and the home `sim`
  test `names_a_complete_reader_once_when_it_misses_a_frame_with_none_waiting` reach it,
  and `does_not_name_a_complete_reader_that_misses_a_frame_while_one_waits` checks that
  a miss while a frame waits gives no wake. The hub `sim` test
  `gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window` checks that
  one commit of about three windows gives a reader each frame, with no hang. The hub
  `sim` test `ends_a_complete_reader_after_its_waiting_frames_when_it_misses_a_frame`
  checks that a reader that takes the first frame of a commit inside its window, then no
  frame until a later commit past its window ends, gets each frame before the miss, then
  `Ended::Behind`. Decided by `laptop.architect` (2026-10-08T07:23:30Z:
  https://github.com/synnaxlabs/foundation/issues/1170#issuecomment-6054827276), which
  supersedes the hub test of
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6033084119.
  Corrected by `laptop.architect` (2026-10-11T11:09:17Z:
  https://github.com/synnaxlabs/foundation/pull/2390#issuecomment-6108391183; the source
  of each part and Q21, 2026-10-11T11:41:19Z:
  https://github.com/synnaxlabs/foundation/pull/2390#issuecomment-6108643775):
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6033084119 said that
  no hub test reaches that wake, and the ruling of #1170 gave the two hub `sim` tests as
  they were before #2388. The ruling of #1718 above supersedes Q21 ("no change now") of
  the same #1133 comment.
  Amended (#2649): a live frame that finds no room in the buffer is a miss too, and
  so is each frame that waits for a grant then, as READER RULES states
  (`laptop.architect`, 2026-10-14T02:10:24Z and 02:12:26Z:
  https://github.com/synnaxlabs/foundation/issues/2649#issuecomment-6159325971 and
  https://github.com/synnaxlabs/foundation/issues/2649#issuecomment-6159349684). The hub
  `sim` test
  `gives_a_complete_reader_that_sleeps_past_its_window_or_its_ring_each_applied_frame`
  checks the frame past the window, and, after #2553, that a frame lost for no room ends
  nothing. A seq lost for no room stays a miss under the catch-up of #274 B1: a reader
  that reaches it ends with `Behind`, never a gap, and the B1 plan states how it knows
  that seq. Superseded by #2553 (READER RULES): a seq lost for no room is no miss, and
  the next frame gives it as its gap (`laptop.architect`, 2026-10-15T18:12:25Z:
  https://github.com/synnaxlabs/foundation/issues/2553#issuecomment-6199035922).
  Supersedes item 4 of
  https://github.com/synnaxlabs/foundation/issues/2649#issuecomment-6159325971
  (`laptop.architect`, 2026-10-14T02:29:09Z:
  https://github.com/synnaxlabs/foundation/issues/2649#issuecomment-6159543609).
  The hub `sim` tests `drops_the_commit_it_waits_for_with_the_hub` (64 values),
  `ends_a_complete_reader_that_holds_a_frame_past_its_window_at_the_next_commit`,
  `gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window`, and
  `ends_a_complete_reader_after_its_waiting_frames_when_it_misses_a_frame` (32 values
  each) hold the hub tasks through a commit, then let the `sim` value pick whether main
  or the commit task runs first, and check that both orders ran (#2388). A `sim` test in
  which the two sides of a wake are ready at one instant, over 32 values of `seed` or
  more, meets the loom and shuttle rule of `docs/claude/testing.md` when its PR states
  how many of the values ran each order. Decided by `laptop.director`
  (2026-10-11T08:25:27Z:
  https://github.com/synnaxlabs/foundation/pull/2318#issuecomment-6107110442), for these
  tests (2026-10-11T10:05:33Z:
  https://github.com/synnaxlabs/foundation/issues/2388#issuecomment-6107876336).
  The three hub `serve` tests of the wake of a session that missed a frame with none
  waiting run, at each value, one order: the serve task waits, then the wake wakes it.
  The other order needs a message of the peer at the instant of the miss commit, which
  the transport times. In that order the serve task polls because of the message, so the
  wake does no work. So the three tests pin the wake as they are, in the one order in
  which it does work. Decided by `laptop.director` (2026-10-11T10:55:43Z:
  https://github.com/synnaxlabs/foundation/pull/2390#issuecomment-6108286003).
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
  Amended (2026-10-08T20:07:32Z, #340 PR 4d-b): a reader of an index whose home is
  another node reads from that home over one hub stream (HUB WIRE), and
  `reader::Error::Remote` goes. `hub::Config::region: Option<hub::Region>` replaces
  `hub::Config::mesh`: a `Region` holds the mesh and the shard's transport, so a mesh
  with no transport is a state the type cannot hold. Each remote reader opens its
  stream on the session that `transport::Transport::dial` gives at that open, the one
  session of the shard to the home (ONE SESSION PER PEER). When that session closes with
  `Code(0)` before the home's `Opened`, as a session that loses the tie-break of ONE
  SESSION PER PEER does, the task of the remote reader dials again and opens on the
  session that this dial gives, at most twice. The home's node dials only when it has no
  open session, and runs one dial at a time, so its session beats at most the session
  that was open and one that this node dialed before that session arrived. A third loss
  needs a session of the home's node to end first, and fails the open with
  `Error::Transport`. The home never served an open on a losing session: the lower node
  holds its streams until it closes it. Amended by `laptop.architect`
  (2026-10-09T09:12:45Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6077974207; "the task
  of the remote reader", 2026-10-09T09:25:27Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6078169582).
  Supersedes the one more dial of
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6077794101. A complete
  reader sends `Credit` once its grant is half a window (512 KiB) short of the frames
  given back plus a window. A task on `hub::Config::tasks` takes each frame off the
  stream of a remote reader as it arrives, so the node takes each byte that it let the
  home send (STREAM WIRE), and an idle caller never holds the window of its session. The
  task also dials the home, sends the open, and sends each `Credit`, so no message that
  waits for room in the session waits for the caller. A complete reader queues at most
  its grant plus one frame, and a frame that starts once the charges that arrived reach
  the grant ends the session with `Refusal::Malformed`. A latest reader keeps only the
  newest frame. Lost: a bound on the sum of the credit at one home; a receive window for
  each stream in `transport`; one task for each session. Trigger: a link that a remote
  latest reader with an idle caller fills, as measured, then a credit of one frame for a
  latest reader (HUB WIRE). Decided by `laptop.architect` (2026-10-09T03:24:37Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6073632038), and the
  open in the task by `laptop.architect` (2026-10-09T06:07:04Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6075355092). The new
  errors: `reader::Error::{Transport, Refused, Message, Pool}` and
  `reader::Ended::{Stream, Refused, Message, Frame, Pool, Credit}`. Each `Refused` holds
  a `wire::hub::Refusal`, the code of HUB WIRE that stopped or reset the stream. A code
  outside HUB WIRE and a failed dial are `Transport` or `Stream`. A stream that the
  home finishes before it ends the session is `Message(Unfinished)` inside a body and
  `Message(Finished)` at each other point, which `wire::hub::Reader::end` gives.
  Decided by `laptop.architect`: the reader's errors (2026-10-07T23:31:29Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6048960511, which
  approves the plan in
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6048861311),
  `Region` (2026-10-08T20:07:32Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6068108715), which
  supersedes `hub::Config::mesh` of
  https://github.com/synnaxlabs/foundation/pull/1979#issuecomment-6067438821 and
  `hub::Config::transport` of 6048960511, and `Refusal`, `Finished`, `end`, and the
  dial at each open, which supersede `Refused(transport::Code)` of `reader::Error` and
  `reader::Ended` in 6048960511 and the session for each home of that plan
  (2026-10-08T21:19:24Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6069259471). The
  removal of `reader::Error::Remote` in 6069259471 supersedes it in
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066821273.
  `laptop.architect` approved `reader::Ended::Credit` (2026-10-09T03:26:05Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6073648189).
  The remote reader costs `complete wait` +2 ns (27 to 29 ns, +7.4%) and
  `complete grant` +2 ns (64 to 66 ns, +3.1%) on a quiet host
  (https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6069990547), with no
  allocation. Accepted until #2025 by `laptop.architect`
  (2026-10-08T22:34:29Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6070407790). Trigger:
  when #2025 merges, #2031 steps the stream in the task of the remote reader with the
  poll forms, and `complete wait` is at most 5% over 27 ns on a quiet host. The task of
  the remote reader costs, per frame on the whole reader node, +16.2% when the caller
  lags (11129 to 12937 ns for 16 KB frames), +13.1% with 1 sample per series when the
  caller lags, and +1.5% when the caller waits, on a loaded host
  (https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6074115276).
  Accepted until #2031 by `laptop.architect` (2026-10-09T04:13:57Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6074122773).
  Trigger: #2031 runs the three cases on the `remote()` rig for 1f2796b4 and its own
  head on one quiet host in one run, and a case more than 5% over 1f2796b4 needs a new
  P1 judgment of `laptop.architect` (2026-10-09T05:15:21Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6074769168).
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
  so its cost stays the same while other sessions end. The hub finds a local writer or
  reader by its home key, as the shard never gives a key twice, a remote reader by a
  scan of its keys, and a served open by a count of the opens. The home stops carrying
  an index only when its key is not an index
  of the new definitions (`Shard::shed`, HOME SURFACE), and carries it again when it
  returns. A key holds one slot as an index (`channel::Slots`), which never changes, as
  the buffer keys its tails by slot (X42), and one slot as a data channel for each
  sample type. So a reader takes no series of its key of another type, and a rename
  keeps the history of the channel (A4) (`laptop.architect`, 2026-10-09T02:59:13Z:
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6073342524).
  Supersedes the retire at each removal in item 1 of
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6072361464. A move
  of index keeps the data slot, but a reader reads only the frames of its own index, so
  no reader of the new index reads a series written on the old one. The move ends each
  session on the channel, so the cut is never silent (`laptop.architect`,
  2026-10-09T20:03:23Z:
  https://github.com/synnaxlabs/foundation/pull/2175#issuecomment-6088342132). An
  index continues its seq after its key was a data channel, also after a restart
  (`laptop.architect`, 2026-10-09T01:25:59Z:
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6072361464). So a slot
  names one key, role, and sample type, and the newest frame of an index stays the
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
  removed, and code `UNKNOWN`: also a rename, a move to another index, another data type
  at the same key, and a removal that a later call undoes. A client write ends only the
  group of the channel once its writers open (HUB CLIENT, PR 2). While it reads its keys
  and while it waits for the home, the call wakes it, as a peer can send nothing more
  and a removed index can get no home. It reads its removal before it checks each later
  key and after the wait, and then carries the index and opens with no `await` between.
  So its index changes only at a removal, and it waits once. A key that a call changes
  before the open checks it is checked against the new definitions. While a local open
  waits for the home, it is registered on each channel that its check gave, as a served
  open is, and on no other index. A spec that removes an index removes or moves each
  data channel on it, and the open takes nothing of an index but its key, which a rename
  keeps. A later spec in use that removes or changes one of its channels wakes it at
  once. It then checks its names, or matches its selector, again, and waits for the home
  of the index that the check gives. A moved channel then opens on its new index. A
  writer whose name is gone fails with `Unknown`. A reader gets what an open at that
  instant gets: its channels, `Empty`, or `ManyIndexes`. A spec that only adds a channel
  wakes no open. When the home comes, a writer carries each index and opens with no
  `await` between, because no channel of its check changed. A reader matches its
  selector again, as an open at that instant does, so it also gets a channel that a spec
  added on its index. It then carries the index and opens with no `await` between. A
  served read selects by name and waits as a local reader does, so it follows the change
  too (`laptop.architect`, 2026-10-09T05:24:32Z:
  https://github.com/synnaxlabs/foundation/issues/2112#issuecomment-6074871156;
  2026-10-09T00:55:16Z:
  https://github.com/synnaxlabs/foundation/pull/2040#issuecomment-6072038723;
  2026-10-09T20:41:35Z:
  https://github.com/synnaxlabs/foundation/issues/2167#issuecomment-6088904511;
  2026-10-09T23:20:10Z:
  https://github.com/synnaxlabs/foundation/issues/2167#issuecomment-6090920229;
  2026-10-12T15:41:55Z:
  https://github.com/synnaxlabs/foundation/issues/2167#issuecomment-6127582652;
  2026-10-12T20:45:34Z:
  https://github.com/synnaxlabs/foundation/pull/2491#issuecomment-6132311572; and
  2026-10-12T21:16:28Z:
  https://github.com/synnaxlabs/foundation/pull/2491#issuecomment-6132751271). This
  changes "After it waits for each index, the open (writer, reader, and served) runs its
  check again" in the second comment: a served open and a local writer read their
  removal in place of that check. Supersedes "each channel and its index" in item 2 of
  https://github.com/synnaxlabs/foundation/issues/2167#issuecomment-6088904511, and "An
  open by name still checks its names again after the wait, and waits again for a new
  index" in
  https://github.com/synnaxlabs/foundation/issues/2112#issuecomment-6074871156: a local
  writer checks its names again only after a wake. The call checks the definitions
  before it changes anything: two channels with one key or one name, or a data channel
  whose index is not an index of the definitions, panic. This changes "A known key or
  name panics", "The hub keeps the key, the sample type, and the index of each", and
  "The PR that defines channels at each new spec decides what a known, renamed, or
  removed channel does" in
  https://github.com/synnaxlabs/foundation/issues/1917#issuecomment-6064624349. Lost: a
  session ends at its next call, under which a writer keeps the control of a removed
  index until it calls, and a reader that waits in `next` needs a wake anyway; and the
  home ends the sessions in `shed`, which knows no data channel, so a removed data
  channel needs a second path; and a hub number for each session, a second key for the
  same fact. Decided by `laptop.architect` (2026-10-08T22:24:43Z:
  https://github.com/synnaxlabs/foundation/issues/2020#issuecomment-6070259814;
  2026-10-08T22:09:38Z:
  https://github.com/synnaxlabs/foundation/issues/1957#issuecomment-6070012949).
  Amended (#340) by `laptop.architect` (2026-10-09T05:44Z:
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6075092459): a
  remote reader ends at a removal as a local reader does. The hub keeps each remote
  reader with its keys from before it dials the home, so a removal during the open
  also ends it. A removal clears the frames that wait, so the reader
  gives `reader::Ended::Removed` at its next take, wakes the caller that waits in
  `next`, and drops its stream with code 0, as each end that is not a refusal does
  (HUB WIRE). `Remote::take` reads this end from the queue it already reads, so it adds
  no check per call. Lost: an end only at the home, which closes the session when the
  home's own definitions change, and which the reader may never see when the two nodes
  apply a spec at different times.
  Amended (#2143) by `laptop.architect` (2026-10-09T13:40:05Z:
  https://github.com/synnaxlabs/foundation/issues/2143#issuecomment-6082087720):
  `Hub::writer` waits for the node's first mesh time (`clock::Reader::reach` on
  `hub::Config::time`, the reader of the home), as it waits for a home, so a hub caller
  never sees `home::writer::Error::Unsynced`. The open first resolves its channels, so
  an unknown channel fails at once. It then waits, and only then checks each home in its
  loop, so it sees a change of the definitions during the wait, and the wait never comes
  between a home check and the carry. The home check of the open ends only after a pass
  over each index that waited for no home, because a home can move while the open waits
  for another (#2164) (PR #2161, rounds 1 to 3, approved by `laptop.architect`,
  2026-10-09T17:50:57Z:
  https://github.com/synnaxlabs/foundation/pull/2161#issuecomment-6086275768).
  Supersedes "in the loop of `Writer::open`, before its second `resolve`" of item 1 of
  https://github.com/synnaxlabs/foundation/issues/2143#issuecomment-6082087720.
  A task gets mesh time from `writer::Writer::now`, which gives `home::Shard::now`
  (HOME CLOCKS) and cannot fail, because mesh time stays once known. Lost:
  `Hub::now() -> Option<Stamp>`, because each caller holds a writer and would `expect`
  mesh time itself. Lost: a `hub::clock` export of `clock::Reader`, because each task
  would compute the midpoint again, and a change of the home's rule would make its
  stamps `Ahead`. Connector time sync adds what it needs with its own caller. Lost: a
  writer that stamps the frame, because only the caller knows when it read each sample.
  `writer::Writer::time` gives a `hub::Time`, which a caller keeps apart from the
  writer: `Time::now` reads what `Writer::now` reads, also after the writer drops. The
  open62541 loop of the opcua kind reads it inside each client call, while the kind
  holds its writer to write. Lost: the writer in `Rc<RefCell<_>>` in the kind, because
  it adds a borrow to each draft and write. Decided by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/1992#issuecomment-6147197858,
  2026-10-13 13:56 UTC).
  `Hub::time` gives a `hub::Time` to a task with no writer, such as the TLS check of
  a kind that only reads. It waits for the node's first mesh time, as the open of a
  writer does, so `Time::now` cannot fail. Lost: `Time::now` that gives
  `Option<Stamp>`, or a `Time::known` beside it, because each caller then handles a
  case that no writer has. `writer::Time` moved to `hub::Time`, because a hub gives
  it. It has no bound: the caller races it against its own clock. Supersedes the "No
  `Hub::time`" of
  https://github.com/synnaxlabs/foundation/issues/1992#issuecomment-6147197858.
  Decided by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/2702#issuecomment-6207425675,
  2026-10-16 03:28 UTC;
  https://github.com/synnaxlabs/foundation/issues/2837#issuecomment-6209931375,
  2026-10-16 06:56 UTC).
  `writer::Writer::entries` gives the entry of each channel of
  `writer::Config::channels`, in that order: its position in the entries of
  `Writer::set`, as `Writer::draft` and `Draft::series_mut` take it. A channel named
  twice has the same entry twice. A layer 3 caller knows only names, so each writer of
  a kind needs this map. `Writer::open` computes it once, because the key set of a
  writer never changes. Lost: `Writer::entry(&Name) -> Option<usize>`, because each
  caller built `Config::channels`, so it knows the order, and a name not in the config
  is a defect of the caller. Lost: a `hub` draft that takes the position of a channel
  in `Config::channels`, because it would repeat each method of `Draft`
  (`laptop.architect`, 2026-10-09T18:47:52Z:
  https://github.com/synnaxlabs/foundation/issues/2170#issuecomment-6087166096).
  A reader opens from `hub::reader::Config { select, mode, subject, name, hold }`: it
  reads each channel whose name `select` matches at the open, which share one index, or
  the open gives `ManyIndexes` with the least matched name and the least name on another
  index (`laptop.architect`, 2026-10-08T06:18:32Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6053699496). A
  reader gives the name of each channel that it reads, as at its open, through
  `Received::name`. The reader of a client read holds no name (`laptop.architect`,
  2026-10-13T01:00:24Z and 01:03:02Z:
  https://github.com/synnaxlabs/foundation/issues/2508#issuecomment-6135761867 and
  https://github.com/synnaxlabs/foundation/issues/2508#issuecomment-6135803509). This
  supersedes "and an open of a connector clones no name" of finding 3 of
  https://github.com/synnaxlabs/foundation/pull/2425#issuecomment-6111681323. Each open
  of `Hub::reader` now copies the bytes of each matched name once, into one text, with
  a count of allocations that does not grow with the channels. A selector that matches
  no channel gives `Empty` until #1819 (`laptop.architect`,
  2026-10-08T06:18:32Z and 06:37:34Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6053699496 and
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6054016051). A named
  reader has one session for each subject and name at the home of its index
  (`laptop.architect`, 2026-10-08T10:15:04Z:
  https://github.com/synnaxlabs/foundation/issues/1851#issuecomment-6057659053, and
  `laptop.architect`, 2026-10-09T21:04:47Z:
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089237192): a later
  open takes over the session, which ends with `Ended::Replaced` before the frames that
  wait for it, as the home closed it (`laptop.architect`, 2026-10-09T21:04:47Z:
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089237192).
  Supersedes the plan test "after the frames that wait for it" of
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6057332703. A named
  complete reader that opens while the home holds its position resumes at the position
  that the acks of its last complete session recorded, or where that session opened
  when they recorded none. It ends with `Ended::Behind` when a frame after that
  position was released (`laptop.architect`, 2026-10-09T21:04:47Z and 21:34:41Z:
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089237192 and
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089638162), or
  dropped because no complete session on its index was open (`laptop.architect`,
  2026-10-09T21:53:40Z:
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089887271;
  `laptop.architect`, 2026-10-08T10:01:19Z and 2026-10-10T00:57:58Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6057419592 and
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091895275). A
  frame lost because the buffer had no room is not in the condition: the reader gets
  the next frame with the lost seqs as its gap (#2553, `laptop.architect`,
  2026-10-15T18:12:25Z:
  https://github.com/synnaxlabs/foundation/issues/2553#issuecomment-6199035922), which
  supersedes `laptop.architect`, 2026-10-14T02:29:09Z:
  https://github.com/synnaxlabs/foundation/issues/2649#issuecomment-6159543609.
  Supersedes the line of
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089887271 that a
  frame lost for no room is not in the condition.
  Supersedes the resume where the last complete session opened, of
  https://github.com/synnaxlabs/foundation/pull/2183#issuecomment-6089237192.
  Amended (#1742 PR 3): `Received::position` gives the reader's position after each
  frame, and `Reader::ack` records it at the home. `Position` holds its index, and an
  ack of another index panics. The ack of a latest reader, of a reader whose index has
  its home at another node, or of a reader that ended (after `next` gave an `Ended`, or
  once the hub ended it with `Removed` or `Replaced`) changes nothing
  (`laptop.architect`, 2026-10-10T00:20:20Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091530673). An
  ack at or below the reader's last ack changes nothing too, and `Reader::ack` gives no
  error: in `delivery`, each path keeps the higher seq, and `Error::Ack` names only a
  dropped or added path (`laptop.architect`, 2026-10-10T00:57:58Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091895275).
  Supersedes the `Errors` section and item 3 of
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091530673, and
  the test "An ack that moves back gives the exact error of `delivery` (`Error::Ack`)"
  of #1742.
  `Reader::ack` panics on a position of another index than the reader's, or past each
  position that `Received::position` gave for the reader. Before it gave one, each
  position panics (`laptop.architect`, 2026-10-10T01:40:23Z:
  https://github.com/synnaxlabs/foundation/pull/2208#issuecomment-6092284036).
  Supersedes item 1 of
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091931740. A
  position that another reader on the index gave, at or below the highest position
  that `Received::position` gave for the reader, is a true ack (`laptop.architect`,
  2026-10-10T01:01:30Z and 2026-10-10T01:40:23Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091931740 and
  https://github.com/synnaxlabs/foundation/pull/2208#issuecomment-6092284036).
  Supersedes the test "An ack of another reader's `Position` panics" of
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6091357396.
  Trigger: #1155 sends the ack of a reader to a home at another node.
  A named open before mesh time gives `Unsynced`. A hold on an unnamed or latest reader
  panics (`laptop.architect`, 2026-10-08T10:01:19Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6057419592). A named
  reader whose index has its home at another node gives `reader::Error::Remote { home }`
  (`laptop.architect`, 2026-10-09T19:33:23Z:
  https://github.com/synnaxlabs/foundation/issues/1742#issuecomment-6087879895).
  Trigger: #1155 removes `reader::Error::Remote` when a named reader opens across nodes.
  Amended (#2185): `Hub::new` states when the hub lets go of the region, so `node` cites
  the hub and not one of its tasks (`laptop.architect`, 2026-10-09T21:38:13Z:
  https://github.com/synnaxlabs/foundation/issues/2185#issuecomment-6089685674). The hub
  holds the home and `hub::Config::region` until the hub and each value and future that
  it gave, directly or through another such value, drop (`laptop.architect`,
  2026-10-09T23:21:55Z, the class in place of a list of holders:
  https://github.com/synnaxlabs/foundation/pull/2197#issuecomment-6090938158). Each task
  of the hub ends at its next poll after that, holding neither. The test
  `remote::lets_go_of_the_transport_once_the_hub_each_session_and_each_link_drop` drops
  a writer and a `Link` beside a remote reader, and asserts that the hub lets go of the
  transport at the next poll of its tasks, and that the transport has no other holder
  once the region drops. Lost: `Hub::ended`, because the event that `node` waits for is
  the free port, which the transport gives, so a hub future adds a surface whose one
  caller must still wait for the transport (`laptop.architect`, 2026-10-09T21:38:13Z:
  https://github.com/synnaxlabs/foundation/issues/2185#issuecomment-6089685674).
  Amended (#274 B1): a complete session reads a frame that it missed at its window later
  from disk, in order. `home::Shard::take` gives `Next::Disk`, and the session runs
  `disk::Read::read`, gives the result to `Shard::fill`, then takes again. `Behind` has
  one meaning: the session reached a seq that the buffer lost for no room. A read from
  disk that fails ends the session with `Ended::Buffer` and its error, and a read or a
  fill that finds no block ends it with `Ended::Pool`, on that call and each later one.
  `home::reader::disk::Error` holds the two errors of a read, as `hub` takes `buffer` as
  a dev-dependency only (`laptop.architect`: the plan, 2026-10-15T16:29:36Z:
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6197039098; its
  changes, 2026-10-15T16:38:55Z, 17:00:40Z, and 17:16:20Z:
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6197223487,
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6197654802, and
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6197956499, whose
  name change it withdrew at 17:31:32Z:
  https://github.com/synnaxlabs/foundation/issues/274#issuecomment-6198252050;
  `disk::Read` and `disk::Error` in place of `Disk` and `Failure`, 2026-10-15T18:23:12Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6199246153, item 2).
  The hub tests named above that checked `Behind` at a miss now check each frame from
  disk, under new names: `slow_reader_gets_every_frame_from_disk`,
  `gives_a_complete_reader_that_holds_a_frame_past_its_window_each_later_frame`,
  `gives_a_complete_reader_its_waiting_frames_then_the_missed_ones_from_disk`,
  `ends_a_complete_reader_that_sleeps_past_its_ring_as_behind`, and the `serve` tests
  `sends_behind_and_finishes_when_a_complete_session_misses_a_lost_frame` and
  `sends_each_frame_in_order_to_a_session_that_misses_frames_before_its_credit`. The two
  `gives_` tests keep their 32 values and their check of both orders. A session decodes
  each entry that it reads from disk onto the key set of its index's group, which it
  interns at its first read. A series whose channel the set no longer has drops, as the
  reader's mask drops it, and a series whose type changed since it was stored drops too:
  the frame carries no series of that channel
  (`resumes_a_named_complete_reader_from_disk_with_no_series_of_a_changed_type`;
  `laptop.architect`, 2026-10-15T18:23:12Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6199246153, item 1).
  `delivery::Readers::fill(key, frame, set, seq)` takes the decoded frame by value with
  its set lent, and `delivery::Readers::reached(key, next, durable)`, which changes
  nothing when `next < durable`, records the end of a read (`laptop.architect`,
  2026-10-15T18:27:16Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6199323861, items 5
  and 6).
  Amended (#2725, with #2723 and #2553): no case gives `Behind` now. A complete session
  that misses a frame reads it from disk, and a seq lost for no room comes as the gap of
  the next frame with a sample (`laptop.architect`, 2026-10-16T02:09:51Z:
  https://github.com/synnaxlabs/foundation/pull/2794#issuecomment-6206458946). The
  variants stay until #2790 removes them (`laptop.architect`, 2026-10-16T00:46:53Z:
  https://github.com/synnaxlabs/foundation/issues/2725#issuecomment-6205434126).
  Amended (#2790): the `Behind` variants are removed (HUB WIRE).
  Amended (#2897): below `durable`, `reached` moves the session's read from disk to
  `next`, also when the read gave no frame. A read that passed lost seqs and then only
  entries with no frame, such as a handoff, read the same entries again with no end
  (`laptop.architect`, 2026-10-16T16:49:39Z:
  https://github.com/synnaxlabs/foundation/issues/2897#issuecomment-6220365677).
  Supersedes "which changes nothing when `next < durable`" above
  (https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6199323861, item 6).
  A `next` below the `from` of the last `Next::Disk`, the `next` of an earlier
  `reached`, or the end of a frame that `fill` gave, is a defect of the caller, and
  `reached` panics (`laptop.architect`, 2026-10-17T04:33:05Z:
  https://github.com/synnaxlabs/foundation/pull/2986#issuecomment-6229674613, and
  2026-10-17T04:55:38Z:
  https://github.com/synnaxlabs/foundation/pull/2986#issuecomment-6229874537). A frame
  given to `fill` that ends at or below any of those seqs is a defect of the caller
  too, and `fill` panics (`laptop.architect`, 2026-10-17T04:51:06Z:
  https://github.com/synnaxlabs/foundation/pull/2986#issuecomment-6229838366, and
  2026-10-17T04:55:38Z:
  https://github.com/synnaxlabs/foundation/pull/2986#issuecomment-6229874537).
  Amended (#2989): `delivery` keeps the whole read cursor of a complete session on
  disk, a `types::log::Mark`, which moves from `buffer` with no re-export.
  `Next::Disk { from }` gives it, and `reached(key, next, durable)` takes the `next` of
  the read whole, so a read goes on past the entries with no samples at a seq that the
  read before it gave. A frame that `fill` gives moves the cursor to `Mark::at` its
  end. The commit wait keeps the whole mark, so a read that stops at the durable end
  inside the entries with no samples at a seq goes on past them after the commit. The
  panics of `reached` compare marks in their order. `home` keeps no mark: it asks
  `delivery::Readers::on_disk` before it decodes a read, and drops the read of a
  session that is not on disk (`laptop.architect`, 2026-10-17T06:49:04Z:
  https://github.com/synnaxlabs/foundation/issues/2989#issuecomment-6230866460, and
  2026-10-17T07:50:29Z:
  https://github.com/synnaxlabs/foundation/pull/3019#issuecomment-6231397750).
  Amended (#2884): the one commit wait of a complete session is at its open.
  `Readers::open` takes `until`, the commit count that the session waits for, and
  `Readers::release(durable, commits)` ends the wait. A read that reaches the durable
  end below the released end is a defect of the caller, and `reached` panics
  (`laptop.architect`, 2026-10-18T09:41:10Z:
  https://github.com/synnaxlabs/foundation/issues/2978#issuecomment-6248353319).
  Supersedes "The commit wait keeps the whole mark" above.
