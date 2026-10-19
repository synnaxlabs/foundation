- **HUB CLIENT (2026-10-08)** `program::Session` is the session of a program with one
  node (the crate `program` and its names, `laptop.architect`, 2026-10-15T18:23:47Z,
  https://github.com/synnaxlabs/foundation/pull/2724#issuecomment-6199257488).
  `Session::connect` dials on a `transport::Client`, sends the subject's hello, and
  gives the session once the node admits it, so a refused hello is an error of
  `connect`. A hello expires at the latest mesh time of its challenge, plus the time
  since the challenge came on the monotonic clock, plus `program::LIFE` (10 minutes), or
  at the end of mesh time when the sum passes it, as mesh time is the node's. Lost: an
  error of `connect` for a challenge near the end of mesh time, a second guard of the
  node's own checks of the expiry (`EXPIRED`). A task renews the hello at half of `LIFE`
  after each admission, on the hello stream, until the session ends. A renewal waits for
  a block as each message does, and the node closes the session if the hello ends first.
  A challenge that `wire` refuses ends the renewal and closes the session with
  `MALFORMED`. From then on, each request that fails gives the error that ended the
  renewal, also a request that was in flight, in place of the error of the close
  (`laptop.architect`, 2026-10-09 01:14 UTC,
  https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6072238837).
  `request(body)` signs the body, sends it on its own stream, and gives the body of the
  response. Requests of one session go one at a time, in the order they began, by a turn
  in the session, as a link holds one open request (HUB LINK). A request dropped before
  its response began keeps the turn until the response begins or the stream ends,
  because the node holds it open until then. `Config` holds its own `pool`, which a
  program may share with the transport. Each message that the session sends takes a
  block from it, which the stream holds until the node acknowledges it. A message that
  finds the pool full waits for a block, so a small pool makes the session slower, not
  fail. It tries again every 10 ms, the span of the transport's reads, and the wait ends
  when the session closes, with the error of the close. Only `Exhausted` and `Refused`
  wait: `TooLarge` never succeeds, so it gives `Error::Pool` at once, which means that
  no block of `pool` can hold a message that the session sends. Lost: a bound of two
  windows from the stream credit, which ack loss can exceed; a `Bytes` body with no
  copy; a wait in `block::Pool` that wakes at each free. Four callers poll a pool now,
  all in a program: the transport's reads, the session's sends on `pool`, the frames
  of a read on `frames`, and the drafts of a writer on `frames`. The trigger fired with
  the third (#2802; `laptop.architect`, 2026-10-16T03:39:42Z,
  https://github.com/synnaxlabs/foundation/pull/2802#issuecomment-6207544466): #2811
  adds the wait that a free wakes to `block`.
  `Session` is `Clone`, and a clone is the same session. When the last clone and the
  last `Reader` or `Writer` drop, it closes the session with `Code(0)`. A node's stop
  and close with a code are one error: `Error::Refused(wire::hub::client::Refusal)`, a
  closed set in `wire` with `from_code` and `code`, of each code that a node stops a
  client stream or closes a client session with (`MALFORMED`, `BUSY`, and `REFUSED` to
  `CHANGED`). Each meaning of a code is stated once, in `wire`, and its `Display` gives
  the cause. A close with 0 or a code outside the set stays `Error::Transport`. Lost:
  one dial for each request (a handshake for each request, and a read stream of HUB LINK
  needs a session that lives; `laptop.architect`, 2026-10-13T02:42:31Z,
  https://github.com/synnaxlabs/foundation/pull/2528#issuecomment-6137032724); `&mut
  self` with the renewal inside `request` (an idle program loses its session at the
  expiry); a queue in a task that owns the session; a lock of a library
  (`tokio::sync::Semaphore` is FIFO, but it is built for many threads, with atomics and
  an `Arc` for an owned permit, where `program` needs one lock on one thread, and
  `futures::lock::Mutex` wakes the first waiter of its slab, not the oldest, and lets a
  new `lock` take the lock ahead of it, which starves a waiter); `Error::Stopped` with a
  `u32` code, which gives `program` a meaning for each code. The plan
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6069186434) was
  approved with the `Refusal` change by `laptop.architect` at 2026-10-08T21:25:05Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6069345374).
  `laptop.architect` approved the surface and the shared `pool` at 2026-10-08T21:32:05Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069455056) and
  21:36:08Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069514013), and the
  order "in the order they began" and the dropped request at 21:38:44Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069553753). It
  approved the `MALFORMED` close at 22:04:03Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069924128). It gave
  the reason against `tokio::sync::Semaphore` at 22:15:33Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070115889). It
  approved the wait for a block at 22:56:10Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070683319). It
  approved the expiry at the end of mesh time at 23:27:59Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6071068010) and its
  Lost clause at 23:29:46Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6071087807).
  Supersedes the sentence "Any pool works" of
  https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069455056, and the
  room of the pool approved at 22:04:03Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6069924128), at
  22:34:24Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070406709), at
  22:43:35Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070527522), and at
  22:46:20Z
  (https://github.com/synnaxlabs/foundation/pull/2009#issuecomment-6070562330).
  `Session::reader(select, mode)` opens a read stream as the subject and gives a
  `Reader`, whose `next` gives each frame that the node sends, in order. A read takes no
  turn: it runs beside the requests. A reader holds a clone of its session. It keeps
  the error that ended it and gives it again at each `next`, and the error that ended
  the renewal replaces it, as for `request`. The key set of a reader is of no node:
  `wire::hub::client::Reader` builds it with `KeySet::ordered` at the last place, so
  entry `n` is place `n`, its slots are not the node's, and places that name a channel
  twice give `client::Error::Repeated` (`laptop.architect`, 2026-10-16T03:12:49Z, item
  1 of https://github.com/synnaxlabs/foundation/pull/2802#issuecomment-6207214002;
  #2808). Its key is `u32::MAX`, and `Interner::intern` panics before it gives that
  key, so a lens that `hub` matches by `set.key()` never takes it for a node's set
  (`laptop.architect`, 2026-10-16T06:47:02Z,
  https://github.com/synnaxlabs/foundation/issues/2808#issuecomment-6209795527).
  `Frames::open` takes the end of the stream as `None` (`laptop.architect`,
  2026-10-16T06:47:02Z,
  https://github.com/synnaxlabs/foundation/issues/2808#issuecomment-6209795527). So
  does the decoder, and a message that the decoder refuses leaves it as it was
  (`laptop.architect`, 2026-10-16T07:19:58Z,
  https://github.com/synnaxlabs/foundation/pull/2839#issuecomment-6210275086). The plan
  (https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6204280445) was
  approved by `laptop.architect` at 2026-10-15T23:19:38Z
  (https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6204306531).
  A frame takes a block of `Config::frames`, a pool apart from `pool` and from the
  transport's, and holds it until it drops. So frames that the program holds never stop
  the renewal of the hello (`laptop.architect`, 2026-10-16T03:12:49Z,
  https://github.com/synnaxlabs/foundation/pull/2802#issuecomment-6207214002). A frame
  that finds `frames` full waits as a message does (#2810 ends a wait that the carve of
  another size blocks), and a frame that no block of `frames` can hold gives
  `Error::Pool` at once. Lost: one
  reserved block for the renewal, a second pool that the caller cannot see.
  Amended (#2829): a complete `program::Reader` goes on past seqs that the node's
  buffer lost. `Reader::next` gives only frames. `Reader::lost(entry)` gives the seqs
  of the index lost after the last series of `entry` with a sample up to the frame
  that the last `next` gave, as one range from the first lost seq to the last, and an
  empty range when that frame holds no sample of `entry` or none was lost. The entry of
  the index gives the gap of the frame. The reader writes one `tracing::warn!` for each
  gap of the index, with the index's name and the lost seqs as fields. A latest reader
  gives no gap. No strict mode: a program that must stop at a loss checks `lost`.
  `wire::hub::Gaps` holds the rule of the gap of a frame, and `wire::hub::Frames` and
  the reader of `hub` each call it. The person decided the shape (2026-10-16T05:22:29Z:
  "We need to preserve the sanctity of the python clients nominal path"; 05:23:50Z, on
  a gap on each channel's data with a warning by default: "Yes that's fine";
  https://github.com/synnaxlabs/foundation/issues/2829#issuecomment-6208729798). Lost:
  an item that is a frame or a gap, which breaks the nominal path. The names, the doc,
  and the warning are by `laptop.architect` (2026-10-16T05:26:47Z,
  https://github.com/synnaxlabs/foundation/issues/2829#issuecomment-6208760389). Lost:
  `next` giving `(Frame, Range<u64>)`, and a second `Frame` type that wraps the first.
  The gap of each entry, and `Gaps` in place of a copy of the rule in `hub` and in
  `wire`, are by `laptop.architect` (2026-10-16T09:44:59Z,
  https://github.com/synnaxlabs/foundation/pull/2856#issuecomment-6212746993). Lost:
  one gap for each frame, which a channel that the gap's frame does not hold never
  sees, and two copies of the rule with a test that pins them to each other. A series
  with no sample takes no gap, as `Gaps` keeps the gap of a frame with no sample for the
  next frame with one (`laptop.architect`, 2026-10-16T14:52:46Z,
  https://github.com/synnaxlabs/foundation/pull/2856#issuecomment-6218152463). `Gaps`
  keeps the gap of the last frame, and the lost seqs of each entry stay in a private
  pure type of `program` until a second crate needs them (#2865; `laptop.architect`,
  2026-10-16T10:25:00Z,
  https://github.com/synnaxlabs/foundation/pull/2856#issuecomment-6213418531). Lost: a
  copy of the gap in each caller of `Gaps`, and a public type in `wire` with one caller.
  The Python SDK, when the `sdk` crate exists, gives `frame["tc1"].lost` from
  `lost` of the entry of `tc1`, built only when the caller reads it, and logs one
  WARNING for each gap on its own logger with no setup. It installs no `tracing`
  subscriber (the person and `laptop.architect`, the same comments).
  Amended (#2718): a program writes on a write stream that it opens at its node. Its
  first message is `client::write::Write` (kind 7: the length of the open and the
  signature of a write proof). The open is `write::Open { authority, lease, channels
  }`. The node replies the `Places` (kind 6, as on a read) and their run.
  `client::write::Opening` decodes them, and `client::Reader` holds an `Opening` and
  adds the `hub::Reader` of the session to its start. Then the program sends each frame
  as a `wire::hub::write::Head { time, form, count, series }`, the run of its ends
  (`wire::hub::ends`), and its body. A `Head` of `Time::Node` holds one raw sample of
  each series, and the node writes a stamp over its 8-byte index sample. The node
  replies `wire::hub::write::Reply`: an `Outcome` for each frame (`Taken` or `Lost`
  with its `frame::Range`, or `Refused` with a `Refusal`), and `Stored { frames }`. A
  frame that finds the pool full gets `Refused(Full)` and the stream stays open. An
  untimed frame behind a later stamp of another writer, or of a timed frame of its own
  stream, gets `Refused(Order)`. `client::write` is a public module with no re-export,
  as its `Open` collides with that of the read. Until the forward of BQ12 (#3104), a
  write at a node that is not the home stops with `NOT_HOME`. The plan
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244711745) was
  approved by `laptop.architect` at 2026-10-18T02:34:55Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244779011), and
  the `Head` at 2026-10-18T02:42:50Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244840425). The
  person objected to its design 1, an `Away` that sends the program to the home, at
  2026-10-18T02:39:23Z (the time that `laptop.monitor` gave): "No, I disagree. What
  does cockroachdb do?" (relayed at 02:40:07Z,
  https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244819528).
  So BQ12 holds: the program reaches only its node, and the node forwards the signed
  hello and open to the home (`laptop.architect`, 2026-10-18T02:46:04Z,
  https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244865133). Lost:
  the `Away` with typed addresses, and a `label` and an `untimed` flag in the `Head`,
  which let a `Resend` or an untimed backfill frame be sent.
  Two shape changes from the plan: `Outcome::Taken` and `Lost` carry a
  `frame::Range { seq, count }`, the form of the home's `Outcome` and of the head of
  HUB WIRE, so a reversed range cannot be sent; and `Opening` gives one
  `Start { places, set }`, which keeps the order of the messages in one decoder.
  Approved by `laptop.architect` at 2026-10-18T03:10:22Z
  (https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245051476).
  `Reply::decode` refuses a range of no sample, which no node sends, and one that ends
  past the highest seq, as HUB WIRE refuses such a head. Found in finding 2 of round 1
  (2026-10-18T03:46:28Z,
  https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245328844), and
  approved with the new `hub::Error` variants by `laptop.architect` at
  2026-10-18T03:48:25Z
  (https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245343667).
  The frame messages of a write, `Head`, `Time`, `Reply`, `Outcome`, and `Refusal`,
  are in `wire::hub::write`, as the session codec of a read is in `wire::hub`, so the
  forward of BQ12 uses them with no client type. Their errors are in `hub::Error`,
  which stays `Copy`. One private codec gives the signed first message of a read and
  of a write by kind, and one private module holds `Places`, `Place`, `place`,
  `PLACES_MAX`, and `OPEN_BYTES_MAX`, which `client` exports. At the node,
  `hub::write::Frames` decodes the frames of a write after the places. It refuses
  each head that `Head::encode` panics on and each head of more series than places,
  and `Head` has no public decode. When the program finishes its stream inside a
  frame, `Frames::finish` gives `Error::Cut` with the bytes of the frame that came,
  and the node stops the stream as `MALFORMED`. The home takes no part of a frame and
  gives it no outcome. For a frame that finds the pool full, the node refuses it and
  calls `Frames::skip`, which drops the frame's body and keeps the stream open
  (`laptop.architect`, 2026-10-18T06:08:47Z,
  https://github.com/synnaxlabs/foundation/pull/3148#issuecomment-6246407916). Decided
  by `laptop.architect` at 2026-10-18T03:22:04Z
  (https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245142760), with
  the surface of `Frames` approved at 2026-10-18T05:42:24Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6246204678). The
  plan first dropped a cut frame with no error. `laptop.monitor` refused that silent
  end, because a program finishes inside a frame only through a defect or a crash, and
  a silent drop hides the defect of an SDK. `laptop.architect` gave `Error::Cut` at
  2026-10-18T05:43:30Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6246212678).
  Amended (#2718 PR 2): the node serves a write stream at the home of its index. The
  open holds its `length` in the hub's body budget until it decodes. Then the stream
  holds 16 bytes for each channel of the open and one for its index until the stream
  ends, and its decoded names, 16 bytes and the length of each, until it sends its
  places. Ends or names that do not fit stop with `BUSY` before the writer opens (MEMORY
  BOUNDS). A write whose channels are on more than one index stops with `MALFORMED`
  before the node waits for a home, so a refused open takes no control at the gate. A
  frame of `Time::Node` gets mesh time, or 1 ns after the last stamp that the node gave
  a frame of the stream that the home took or lost, when mesh time is not after it.
  After each commit, the node sends `Stored { frames }` with the count of frames of the
  stream through the last frame that it took before it called the commit. Refused and
  lost frames count, and frames of both paths count in the one count, so a program drops
  the first `frames` of the frames that it keeps. When the program finishes, the node
  waits for the commit of each taken frame, sends its `Stored`, and then finishes the
  stream, so the program learns that each taken frame is on disk. Control ends when the
  program finishes the stream: the writer closes before the node waits for those
  commits. Each frame takes a block for its outcome from the home's pool before it takes
  a draft, and a frame that finds none stops the stream with `BUSY`, so the home took no
  frame past the last outcome. After its body, right before the home takes it, the frame
  takes a block for the next `Stored` when the stream holds none, and a frame that finds
  none gets `Refused(Full)`. A frame that the home does not take, while no taken frame
  waits for a `Stored`, gives that block back, so a stream holds at most two such
  blocks, and none while it idles. So each taken frame has the block of its `Stored`:
  the node sends each `Stored` when its commit ends, also while the pool is full, and
  the finish waits only for the commits. A stream that stops sends no `Stored` after the
  stop (amended by #3207). A write takes a stream of any class: at the node, the class
  sets only the priority and the carrier of its replies. The two key choices, the wait
  at the finish and the ends in the body budget, went to `laptop.monitor` at
  2026-10-18T07:43:08Z, with no objection. Approved by `laptop.architect` at
  2026-10-18T07:45:01Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6247275705) and
  2026-10-18T08:03:14Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6247422979). Lost:
  a count cap on write streams, which bounds no bytes without the widest open. The reply
  block and the rule for `Stored` approved by `laptop.architect` at 2026-10-18T08:45:22Z
  (https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6247800403), and sent
  to `laptop.monitor` as a key choice with that ruling at 2026-10-18T08:53:42Z, with no
  objection. The wait of the last `Stored` until the session closes, not the stream,
  approved by `laptop.architect` at 2026-10-18T09:33:51Z
  (https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6248291282).
  `Stored { frames }`, a count of the frames of the stream, in place of a path and a
  last seq of each path, decided by `laptop.architect` at 2026-10-18T10:09:56Z
  (https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6248601265), who sent
  it to `laptop.monitor` as a key choice.
  Amended (#2718 PR 3): `program::Session::writer` opens a `program::Writer` from a
  `program::write::Config { authority, lease, channels }`, and waits for the places.
  `program::write` re-exports `Outcome`, `Refusal`, and `Time` of `wire::hub::write`.
  The writer holds its one draft: `Writer::draft` gives `&mut Draft`, and
  `Writer::write(time)` sends it and waits for its outcome, so a draft of another
  writer, whose key set has the same key, reaches it only through a swap of two
  `&mut Draft`. A write with no draft panics. The panic was approved by
  `laptop.architect` at 2026-10-18T14:13:30Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6250930232). When
  `sdk` wraps `Writer` (#2177), its write with no draft gives an error, not this panic.
  `Error::Unnamed` refuses a config of no channel, and `Error::Unsent { count, form }` a
  frame of no sample or a frame of `Time::Node` that is encoded or holds more than one
  sample; each sends nothing. `Session::writer` gives `Error::Pool` when no block of
  `Config::pool` holds the head and an end of each place, so a write waits for a block
  and never gives `Pool`. A `write` dropped after its sends loses only its outcome; one
  dropped inside its sends ends the writer with a reset of `Code(0)`, and each later
  call gives that error, so no head lands in a cut body. A dropped `stored` loses
  nothing. The plan
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6250678711) was
  approved, with the draft held by the writer, `Pool` at the open, and the doc of a
  dropped `stored`, by `laptop.architect` at 2026-10-18T13:53:31Z
  (https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6250729056). Lost:
  a `write::Draft` with the number of its writer and an `Error::Foreign`, which adds a
  type of pass-through methods and an error for a case that the held draft makes
  impossible. The same ruling moves the `os` bench of the node cost to PR 4 of #2718, in
  place of condition 2 of
  https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6248331209.
  Amended (#2718 PR 3, round 1): `Writer::draft(form, count, series)` is async. It sets
  the count of group 0, and it waits for a block of a full pool of frames, as a read
  does. It gives `Error::Layout` when the series break a rule of the frame layout:
  nothing was taken, and the writer goes on. `Writer::finish(self)` drops a held draft
  and finishes the stream, so control ends. It gives `Ok` once the last `Stored` covers
  each taken frame and the node finished. Dropping the call drops the writer.
  `wire::hub::write::Replies` checks the order of the replies of a write: an outcome
  with no frame sent for it gives `wire::hub::Error::Unasked`, and a `Stored` below the
  last one or past the outcomes gives `wire::hub::Error::Stored`. Each ends the writer
  with a reset of `MALFORMED`. Decided by `laptop.architect` at 2026-10-18T14:44:41Z
  (https://github.com/synnaxlabs/foundation/pull/3201#issuecomment-6251244519).
  Amended (#2718 PR 3, round 2): `Writer::draft` gives the error that ended the writer
  before it takes a block. `wire::hub::Error::Stored` holds `frames` and `bound`, the
  last `Stored` when `frames` is below it, else the frames that got an outcome, so
  `Error` stays 24 bytes and the node's decoders do not grow. Decided by
  `laptop.architect` at 2026-10-18T15:49:57Z
  (https://github.com/synnaxlabs/foundation/pull/3201#issuecomment-6251902903).
  Amended (#3217): each outcome holds `frame` (`u64`) after its kind: the count of
  frames of the stream through the frame that it answers, from 1, in the unit of
  `Stored { frames }`. Taken and lost encode to 21 bytes, refused to 10.
  `Replies::decode` takes only an outcome of the next frame. Frame 0, or a frame past
  those sent, gives `wire::hub::Error::Unasked { frame, sent }`. Another frame that
  the program sent gives `wire::hub::Error::Order { frame, next }`. So an outcome of
  another frame ends the writer with a reset of `MALFORMED` at any time of its
  arrival. An outcome that names the next frame is its outcome, also when the node
  states it wrong: no field of the wire can tell. The node takes `frame` from its
  count of frame heads, not of outcomes. So an extra outcome that it sends after the
  outcome of a frame and before the next head names a frame that has one. One that it
  sends after a head and before the outcome of that frame names that frame, and the
  program takes it as its outcome. `Writer::write` takes no reply before it sends.
  Key choice (f) went to `laptop.monitor`, with no objection (2026-10-18T18:16:02Z,
  https://github.com/synnaxlabs/foundation/issues/3217#issuecomment-6253408518).
  Decided by `laptop.architect` at 2026-10-18T18:17:20Z
  (https://github.com/synnaxlabs/foundation/issues/3217#issuecomment-6253421664),
  with the surface and frame 0 as `Unasked` at 2026-10-18T18:18:41Z
  (https://github.com/synnaxlabs/foundation/issues/3217#issuecomment-6253435782),
  the limit of an outcome that names the next frame at 2026-10-18T18:43:23Z
  (https://github.com/synnaxlabs/foundation/pull/3240#issuecomment-6253686852), and
  the count of frame heads at 2026-10-18T18:59:25Z
  (https://github.com/synnaxlabs/foundation/pull/3240#issuecomment-6253848770), with
  its text of both cases approved at 2026-10-18T19:56:18Z
  (https://github.com/synnaxlabs/foundation/pull/3240#issuecomment-6254436267).
  Amended (#3207): each frame takes the block of the next `Stored` right before the
  home takes it, and a frame that finds none gets `Refused(Full)`, not a stop with
  `BUSY`, which would drop the `Stored` of each frame taken before it. Supersedes item 2
  of https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6247800403 and the
  wait of the last `Stored` until the session closes
  (https://github.com/synnaxlabs/foundation/pull/3161#issuecomment-6248291282): a
  `Stored` that found no block was not sent during the stream, and the last one waited
  for a block at the finish. A program that waited for a `Stored` with no next frame and
  no finish then waited forever. Approved by `laptop.architect` at
  2026-10-18T18:58:27Z
  (https://github.com/synnaxlabs/foundation/issues/3207#issuecomment-6253839277), and
  accepted by `laptop.monitor` as a key choice at 2026-10-18T18:54:12Z
  (https://github.com/synnaxlabs/foundation/issues/3207#issuecomment-6253796728) and
  2026-10-18T18:58:45Z
  (https://github.com/synnaxlabs/foundation/issues/3207#issuecomment-6253842265). The
  reserve after the body and the release after a frame that the home does not take:
  OK of `laptop.architect` at 2026-10-18T19:40:34Z
  (https://github.com/synnaxlabs/foundation/pull/3247#issuecomment-6254282232). Lost:
  a `Stored` that finds no block tries again every 10 ms, which sends it up to 10 ms
  after a free and keeps a poller of the pool.
  Amended (#3266): `program` re-exports `Place` at its root, and
  `program::Writer::index` gives the place of the index of the writer, so `sdk` needs
  no `wire` to read the places. Lost: a flag of the index on each `Place`, which
  changes `wire` for one fact of the stream. Decided by `laptop.architect` at
  2026-10-18T20:58:27Z
  (https://github.com/synnaxlabs/foundation/issues/3266#issuecomment-6255066811).
  Amended (#3283): the node splits a write by index, not the client. The person, at
  2026-10-19T12:27:44Z
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6267198734):
  "Decision of the person on #3283 (2026-10-19, about 06:45 MDT): the node splits a
  write by index, not the client. The client opens one write stream with channels of
  any index; the node groups each frame by index and writes each group at its home
  (forwarding the parts through the BQ12 forward of #3104 when the home is another
  node). Prior art the person named: the CockroachDB gateway splits a batch by range;
  the Synnax Core splits a frame by index. A write call may hold channels of more than
  one index: check names, types, and lengths before anything is sent, and when a group
  fails after that, the error names the channels written and not written. An error of
  one index group does not end the others." This ends the client split that
  `laptop.architect-2` approved at 2026-10-19T12:32:48Z
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6267292269). The
  plan: https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6267483536.
  PR 1: each `Place` holds `index`, the place of the index of its group, an index its
  own place, approved by `laptop.architect` at 2026-10-19T13:10:34Z
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6268011114), and
  `Places` holds only `count`, from 1 to `PLACES_MAX`. A read has one index, so each of
  its places names it. `Opening` builds the key set with `KeySet::ordered`, and at the
  last place gives `Error::Index` for the first place whose index is not a place whose
  index is itself, then `Error::Unstamped` for an index that is not `Stamp`, then
  `Error::Repeated`. This replaces the lost flag of #3266 above: the index is now a fact
  of each group, not of the stream. Lost: a `Places` with a run of the index places,
  whose length is not fixed and which can pass a message.
  PR 1b: a write frame is `Head { time, form, series }`, the run of its ends, a run of
  counts, and its body. The run of counts holds one `u32` of at least 1 for each group
  whose index entry is among the ends, in group order, in messages of whole counts. A
  head of `Time::Node` has no run of counts, and each of its groups holds one sample. So
  the wire holds no count of counts and no node count, and neither can be wrong. A
  series of a group whose index the ends do not hold gets the layout error
  `IndexAbsent`. Each outcome holds `group` (`u32`) after `frame`, and the outcomes of a
  frame come in group order: `Replies::sent` takes the groups of each frame, and an
  outcome of that frame for another group gives `Error::Group { frame, group, next }`,
  also for a group that the frame does not hold. `Outcome::Ended(End)` (kind 5,
  `End::Removed` or `End::Failed`) answers a group whose writer ended. The ends of a
  write stream are 24 bytes for each place: 16 for its end, and 8 for its group and
  count (MEMORY BOUNDS). Decided by `laptop.architect` at 2026-10-19T13:14:46Z
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6268096380), on the
  revision https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6267978917.
  `Error::Group` with `next`, a unit `Error::Stamp`, and PR 1c, where `Reply::Stored {
  group, frames }` and `Reply::Ended { group, end }` come and which merges before PR 2,
  are by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6268910604). Until
  PR 2, `hub` never gives `Outcome::Ended`. Key choice (f) went to `laptop.monitor`.
  Lost: a count of each group in the head, which no message of a small
  `message_bytes_max` holds for many groups.
  PR 1c: `Reply::Stored { group, frames }` (kind 4, 13 bytes) counts the frames of one
  group on disk, and `Reply::Ended { group, end }` (kind 6, 6 bytes) ends the write of
  one group before each later outcome of that group, which is `Outcome::Ended`.
  `Replies::new` takes the count of groups. The write is stored when each group ended
  or has each taken frame on disk, and `Replies::unstored` gives each group that ended
  with a taken frame not on disk. A reply that names no group of the write gives
  `Error::Groups`, a reply of an ended group other than `Outcome::Ended` gives
  `Error::Ended`, and an `Outcome::Ended` of a group that did not end gives
  `Error::Unended`. In `program`, `Writer::stored` never waits on an ended group: it
  gives `Error::Unstored` with the index place and the end of each such group, and the
  writer goes on. `Writer::finish` gives it after `Finished`. Until PR 2, `hub` sends
  only `Stored` of group 0 and never ends a group. Plan by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6268011114) and
  https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6269218439. Lost: a
  `Stored` of the whole write, which a group that ends can hold back for ever.
  PR 2: the node opens one home writer for each group of a write, in group order,
  once the home of each index is this node, and the write no longer stops with
  `MALFORMED` for many indexes. This supersedes that stop in the amendment of #2718
  PR 2 above. A group whose home is another node stops the open with `NOT_HOME`, with
  no control taken, until #3104. A frame of a stream of one group goes to its writer as
  it is. A frame of many groups is copied into one draft for each group that it holds,
  and a group whose copy finds no block gets `Refused(Full)` or `Refused(Large)` while
  the other groups go on. Each group of a frame of `Time::Node` gets its own stamp.
  Each frame takes one outcome block for each group that it holds before its draft.
  Each group takes the block of its next `Stored` on its own, so a stream holds at most
  two such blocks for each group. A removed channel of a group sends `Ended { group,
  Removed }`, and a `Disk` error or a failed commit of a group sends `Ended { group,
  Failed }`, in the block of its next or due `Stored`. The `Ended` comes before the
  outcome of its frame, each later outcome of the group is `Outcome::Ended`, and no
  `Stored` of the group follows. The other groups and the stream go on. A stream error,
  a bad frame, or `BUSY` still stops every group. Plan
  (https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6269723864 and
  https://github.com/synnaxlabs/foundation/issues/3283#issuecomment-6269769681), sent
  to `laptop.architect`. Lost: one home writer for all groups, which cannot end one
  group, and one block of `Stored` for all groups, which can pass the largest block of
  the pool.
